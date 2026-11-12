//! The QuickJS side of `run_code` and `workflow`: one engine per script on
//! its own thread. The script only reaches the outside world through
//! `__nyq_call`, which hands a tool call to the agent loop (see
//! [`super::drive`]); QuickJS itself has no file, network or process access
//! and no module loader is installed.
//!
//! Protocol: the thread runs JavaScript until no job is pending, then sends
//! [`Event::Idle`]. Every tool call issued since the last idle point forms one
//! batch; the driver runs it through the agent's dispatch and sends back one
//! [`Reply`] per call, which settles the matching promise.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

use rquickjs::{CatchResultExt, Context, Ctx, Exception, Function, Runtime};
use tokio::sync::mpsc::UnboundedSender;

use super::{Kind, Limits};

/// What the script asks of the driver.
#[derive(Debug)]
pub enum Event {
    Call {
        id: u64,
        name: String,
        args: String,
    },
    /// Nothing more runs until a reply arrives.
    Idle,
    Phase(String),
    Log(String),
    /// The script settled: its JSON result, or an error message.
    Done(Result<String, String>),
}

/// A tool call's result for the script.
pub struct Reply {
    pub id: u64,
    pub ok: bool,
    pub payload: String,
}

/// A running script.
pub struct Engine {
    pub events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    replies: Option<mpsc::Sender<Reply>>,
    abort: Arc<AtomicBool>,
}

impl Engine {
    pub fn reply(&self, reply: Reply) {
        if let Some(tx) = &self.replies {
            let _ = tx.send(reply);
        }
    }

    /// Stop the script, even mid-loop. The thread exits on its own.
    pub fn abort(&mut self) {
        self.abort.store(true, Ordering::Relaxed);
        self.replies = None;
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.abort();
    }
}

const PRELUDE: &str = r#"
"use strict";
(() => {
  const call = globalThis.__nyq_call, done = globalThis.__nyq_done;
  const log = globalThis.__nyq_log, phaseMark = globalThis.__nyq_phase;
  const names = JSON.parse(globalThis.__nyq_tool_names);
  for (const k of ["__nyq_call", "__nyq_done", "__nyq_log", "__nyq_phase", "__nyq_tool_names"]) {
    delete globalThis[k];
  }
  const pending = new Map();
  Object.defineProperty(globalThis, "__nyq_settle", {
    value: (id, ok, payload) => {
      const p = pending.get(id);
      if (!p) return;
      pending.delete(id);
      if (ok) {
        p.resolve(payload);
      } else {
        const e = new Error(payload);
        e.name = "ToolError";
        p.reject(e);
      }
    },
  });
  const invoke = (name, args) => new Promise((resolve, reject) => {
    let id;
    try {
      id = call(String(name), JSON.stringify(args === undefined ? {} : args));
    } catch (e) {
      reject(e);
      return;
    }
    pending.set(id, { resolve, reject });
  });
  const tools = {};
  for (const n of names) {
    Object.defineProperty(tools, n, { value: (args) => invoke(n, args), enumerable: true });
  }
  globalThis.tools = Object.freeze(tools);
  const show = (a) => a.map((x) => (typeof x === "string" ? x : JSON.stringify(x))).join(" ");
  globalThis.console = Object.freeze({
    log: (...a) => log(show(a)),
    info: (...a) => log(show(a)),
    warn: (...a) => log(show(a)),
    error: (...a) => log(show(a)),
  });
  const finish = (fn) => {
    Promise.resolve()
      .then(fn)
      .then(
        (v) => {
          let out;
          try {
            out = typeof v === "string" ? JSON.stringify(v) : JSON.stringify(v === undefined ? null : v);
          } catch (e) {
            done(false, "The result is not JSON-serializable: " + e);
            return;
          }
          done(true, out === undefined ? "null" : out);
        },
        (e) => done(false, e && e.stack ? String(e) + "\n" + e.stack : String(e))
      );
  };
  Object.defineProperty(globalThis, "__nyq_main", { value: finish });

  if (globalThis.__nyq_workflow) {
    delete globalThis.__nyq_workflow;
    const LIMIT = 3;
    const pool = async (thunks) => {
      const out = new Array(thunks.length);
      let next = 0;
      const worker = async () => {
        while (next < thunks.length) {
          const i = next++;
          out[i] = await thunks[i]();
        }
      };
      await Promise.all(Array.from({ length: Math.min(LIMIT, thunks.length) }, worker));
      return out;
    };
    globalThis.task = async (goal, opts = {}) => {
      const args = { action: "run", task: String(goal) };
      args.description = opts.description || String(goal).split("\n")[0].slice(0, 60);
      if (opts.schema !== undefined) args.output_schema = opts.schema;
      if (opts.validators !== undefined) args.validators = opts.validators;
      for (const k of ["mode", "isolation", "name", "model", "effort", "permission_mode"]) {
        if (opts[k] !== undefined) args[k] = opts[k];
      }
      const out = await invoke("subagent", args);
      if (opts.schema === undefined && opts.validators === undefined) return out;
      const v = JSON.parse(out);
      return opts.full ? v : v.result;
    };
    globalThis.parallel = (items) =>
      pool(Array.from(items, (x) => (typeof x === "function" ? x : () => x)));
    globalThis.pipeline = (items, fn) =>
      pool(Array.from(items, (item, i) => () => fn(item, i)));
    globalThis.phase = (name) => {
      phaseMark(String(name));
    };
  }
})();
"#;

/// Start `code` (an async function body) on a new thread.
pub fn start(code: &str, kind: Kind, tools: Vec<String>, limits: Limits) -> Engine {
    let (events_tx, events) = tokio::sync::mpsc::unbounded_channel();
    let (replies_tx, replies_rx) = mpsc::channel();
    let abort = Arc::new(AtomicBool::new(false));
    let code = code.to_string();
    let flag = abort.clone();
    let tx = events_tx.clone();
    let spawned = std::thread::Builder::new()
        .name("nyq-script".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let result = run(&code, kind, &tools, &limits, &tx, &replies_rx, &flag);
            let _ = tx.send(Event::Done(result));
        });
    if let Err(e) = spawned {
        let _ = events_tx.send(Event::Done(Err(format!(
            "could not start the script engine: {e}"
        ))));
    }
    Engine {
        events,
        replies: Some(replies_tx),
        abort,
    }
}

fn run(
    code: &str,
    kind: Kind,
    tools: &[String],
    limits: &Limits,
    events: &UnboundedSender<Event>,
    replies: &mpsc::Receiver<Reply>,
    abort: &Arc<AtomicBool>,
) -> Result<String, String> {
    let deadline = Instant::now() + limits.timeout;
    let rt = Runtime::new().map_err(|e| e.to_string())?;
    rt.set_memory_limit(limits.memory);
    rt.set_max_stack_size(1024 * 1024);
    let stop = abort.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        stop.load(Ordering::Relaxed) || Instant::now() >= deadline
    })));
    let ctx = Context::full(&rt).map_err(|e| e.to_string())?;
    let result: Rc<RefCell<Option<Result<String, String>>>> = Rc::default();
    let outstanding = Rc::new(Cell::new(0usize));
    let calls = Rc::new(Cell::new(0usize));
    let next_id = Rc::new(Cell::new(1u64));

    ctx.with(|ctx| -> Result<(), String> {
        let fail = |ctx: &Ctx, e: rquickjs::Error| Err(e).catch(ctx).map_err(|e| e.to_string());
        let g = ctx.globals();
        let tx = events.clone();
        let (out, made, ids, max) = (
            outstanding.clone(),
            calls.clone(),
            next_id.clone(),
            limits.max_calls,
        );
        let call = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, name: String, args: String| -> rquickjs::Result<u64> {
                if made.get() >= max {
                    return Err(Exception::throw_message(
                        &ctx,
                        &format!("tool call limit reached ({max} calls per script)"),
                    ));
                }
                made.set(made.get() + 1);
                let id = ids.get();
                ids.set(id + 1);
                out.set(out.get() + 1);
                let _ = tx.send(Event::Call { id, name, args });
                Ok(id)
            },
        );
        let slot = result.clone();
        let done = Function::new(ctx.clone(), move |ok: bool, payload: String| {
            slot.borrow_mut()
                .get_or_insert(if ok { Ok(payload) } else { Err(payload) });
        });
        let tx = events.clone();
        let log = Function::new(ctx.clone(), move |text: String| {
            let _ = tx.send(Event::Log(text));
        });
        let tx = events.clone();
        let phase = Function::new(ctx.clone(), move |name: String| {
            let _ = tx.send(Event::Phase(name));
        });
        for (name, f) in [
            ("__nyq_call", call),
            ("__nyq_done", done),
            ("__nyq_log", log),
            ("__nyq_phase", phase),
        ] {
            if let Err(e) = f.and_then(|f| g.set(name, f)) {
                return fail(&ctx, e);
            }
        }
        let names = serde_json::to_string(tools).unwrap_or_else(|_| "[]".into());
        g.set("__nyq_tool_names", names)
            .map_err(|e| e.to_string())?;
        if kind == Kind::Workflow {
            g.set("__nyq_workflow", true).map_err(|e| e.to_string())?;
        }
        if let Err(e) = ctx.eval::<(), _>(PRELUDE) {
            return fail(&ctx, e);
        }
        // The body runs inside an async function, so `await` and `return`
        // work at the top level. Syntax errors surface here.
        let wrapped = format!("__nyq_main(async () => {{\n{code}\n}});");
        if let Err(e) = ctx.eval::<(), _>(wrapped) {
            return fail(&ctx, e);
        }
        Ok(())
    })?;

    loop {
        loop {
            match rt.execute_pending_job() {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    let msg =
                        e.0.with(|ctx| ctx.catch().get::<String>().unwrap_or_default());
                    if abort.load(Ordering::Relaxed) || Instant::now() >= deadline {
                        return Err(stopped(limits, abort));
                    }
                    // A job threw outside any promise chain the script awaits.
                    return Err(format!("uncaught exception: {msg}"));
                }
            }
        }
        if let Some(r) = result.borrow_mut().take() {
            return r;
        }
        if abort.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Err(stopped(limits, abort));
        }
        if outstanding.get() == 0 {
            return Err(
                "The script is waiting for a promise that can never settle (no tool call is pending)."
                    .into(),
            );
        }
        let _ = events.send(Event::Idle);
        let wait = deadline.saturating_duration_since(Instant::now());
        let first = match replies.recv_timeout(wait) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(stopped(limits, abort)),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("script aborted".into()),
        };
        let mut batch = vec![first];
        batch.extend(replies.try_iter());
        ctx.with(|ctx| -> Result<(), String> {
            let settle: Function = ctx
                .globals()
                .get("__nyq_settle")
                .map_err(|e| e.to_string())?;
            for r in batch {
                outstanding.set(outstanding.get().saturating_sub(1));
                if let Err(e) = settle.call::<_, ()>((r.id, r.ok, r.payload)) {
                    return Err(e).catch(&ctx).map_err(|e| e.to_string());
                }
            }
            Ok(())
        })?;
    }
}

fn stopped(limits: &Limits, abort: &AtomicBool) -> String {
    if abort.load(Ordering::Relaxed) {
        "script aborted".into()
    } else {
        format!(
            "script timed out after {}s and was stopped",
            limits.timeout.as_secs()
        )
    }
}
