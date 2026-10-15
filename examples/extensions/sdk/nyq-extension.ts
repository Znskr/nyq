// A dependency-free helper for writing nyq extensions in TypeScript or
// JavaScript. It speaks nyq's extension protocol (JSON-RPC 2.0, one JSON
// object per line on stdin/stdout) so an extension is a few lines:
//
//   import { extension } from "./nyq-extension.ts";
//   extension({
//     tools: [{ name: "now", description: "The time", read_only: true, run: () => new Date().toString() }],
//     commands: [{ name: "hi", description: "Greet", run: (args) => `Hello ${args}` }],
//     on: { tool_call: (e) => (e.tool === "bash" && /rm -rf/.test(String(e.input.command)) ? { block: "no" } : undefined) },
//   });
//
// Runs with `node` (22.6+ strips the types; `nyq-extension.mjs` is the same
// code as plain JavaScript for any Node version) or `bun`. The protocol is
// described in docs/extensions.md.

import { createInterface } from "node:readline";

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

/** What `initialize` tells the extension. */
export interface Session {
  nyq_version: string;
  protocol: number;
  cwd: string;
  session_id: string;
  mode: "tui" | "print" | "acp";
}

/** Requests and notifications the extension can send to nyq. */
export interface Context {
  session: Session;
  /** Show a message to the user. */
  notify(text: string, level?: "info" | "warning" | "error"): void;
  /** Ask a yes/no question (false outside the terminal UI). */
  confirm(text: string): Promise<boolean>;
  /** Ask for a line of text (null when skipped or outside the terminal UI). */
  input(prompt: string): Promise<string | null>;
  /** Send a message as the user: after the current task (follow_up) or as steering. */
  send(text: string, queue?: "follow_up" | "steer"): void;
  /** Set (or with "" clear) this extension's status-line segment. */
  status(text: string): void;
  /** Append a line to the extension's log (~/.nyq/logs/ext-<name>.log). */
  log(text: string): void;
}

export type ToolResult =
  | string
  | { content: Array<{ type: "text"; text: string } | { type: "image"; data: string; mime_type: string }>; is_error?: boolean };

export interface ToolDef {
  name: string;
  description: string;
  input_schema?: { [key: string]: Json };
  /** Read-only tools run without asking; others ask like any tool that changes things. */
  read_only?: boolean;
  run(input: { [key: string]: any }, ctx: Context): ToolResult | Promise<ToolResult>;
}

export type CommandResult = string | { message?: string; prompt?: string } | void;

export interface CommandDef {
  name: string;
  description: string;
  args_hint?: string;
  run(args: string, ctx: Context): CommandResult | Promise<CommandResult>;
}

type Maybe<T> = T | void | undefined | Promise<T | void | undefined>;

export interface Handlers {
  session_start?(e: { source: string; cwd: string; session_id: string }, ctx: Context): Maybe<{}>;
  user_prompt?(e: { prompt: string }, ctx: Context): Maybe<{ block?: string; prompt?: string; context?: string }>;
  tool_call?(e: { tool: string; input: { [key: string]: any } }, ctx: Context): Maybe<{ block?: string; input?: { [key: string]: Json } }>;
  tool_result?(e: { tool: string; input: { [key: string]: any }; content: string; is_error: boolean }, ctx: Context): Maybe<{ content?: string }>;
  turn_end?(e: { text: string }, ctx: Context): Maybe<{}>;
  before_compact?(e: {}, ctx: Context): Maybe<{ instructions?: string }>;
  session_end?(e: {}, ctx: Context): Maybe<{}>;
}

export interface Extension {
  tools?: ToolDef[];
  commands?: CommandDef[];
  on?: Handlers;
  /** Initial status-line text (a function runs after `init`). */
  status?: string | ((ctx: Context) => string);
  /** Called once nyq has said hello, before tools and commands are used. */
  init?(ctx: Context): void | Promise<void>;
}

/** Run `ext` until nyq closes stdin. */
export function extension(ext: Extension): void {
  const write = (msg: object) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", ...msg }) + "\n");
  const pending = new Map<number, (value: any) => void>();
  let next = 1;
  const request = (method: string, params: object) =>
    new Promise<any>((resolve) => {
      const id = next++;
      pending.set(id, resolve);
      write({ id, method, params });
    });
  const notify = (method: string, params: object) => write({ method, params });
  const ctx: Context = {
    session: { nyq_version: "", protocol: 1, cwd: process.cwd(), session_id: "", mode: "print" },
    notify: (text, level = "info") => notify("ui/notify", { text, level }),
    confirm: async (text) => (await request("ui/confirm", { text })) === true,
    input: async (prompt) => (await request("ui/input", { prompt })) ?? null,
    send: (text, queue = "follow_up") => notify("session/send", { text, queue }),
    status: (text) => notify("status/set", { text }),
    log: (text) => notify("log", { text }),
  };
  const tools = new Map((ext.tools ?? []).map((t) => [t.name, t]));
  const commands = new Map((ext.commands ?? []).map((c) => [c.name, c]));
  const handlers = { ...ext.on } as { [name: string]: ((e: any, ctx: Context) => any) | undefined };
  let inflight = 0;
  let closed = false;

  async function handle(method: string, params: any): Promise<any> {
    if (method === "initialize") {
      ctx.session = params;
      await ext.init?.(ctx);
      return {
        tools: [...tools.values()].map(({ name, description, input_schema, read_only }) => ({
          name,
          description,
          input_schema: input_schema ?? { type: "object", properties: {} },
          read_only: read_only ?? false,
        })),
        commands: [...commands.values()].map(({ name, description, args_hint }) => ({ name, description, args_hint: args_hint ?? "" })),
        events: Object.keys(handlers).filter((k) => typeof handlers[k] === "function"),
        status: typeof ext.status === "function" ? ext.status(ctx) : ext.status,
      };
    }
    if (method === "tool/call") {
      const tool = tools.get(params.name);
      if (!tool) throw new Error(`unknown tool ${params.name}`);
      try {
        const out = await tool.run(params.input ?? {}, ctx);
        return typeof out === "string" ? { content: [{ type: "text", text: out }] } : out;
      } catch (e) {
        return { content: [{ type: "text", text: String(e instanceof Error ? e.message : e) }], is_error: true };
      }
    }
    if (method === "command/run") {
      const command = commands.get(params.name);
      if (!command) throw new Error(`unknown command ${params.name}`);
      const out = await command.run(params.args ?? "", ctx);
      return typeof out === "string" ? { message: out } : (out ?? {});
    }
    if (method.startsWith("event/")) {
      const handler = handlers[method.slice("event/".length)];
      return (handler ? await handler(params, ctx) : undefined) ?? {};
    }
    throw Object.assign(new Error(`unknown method ${method}`), { code: -32601 });
  }

  const lines = createInterface({ input: process.stdin });
  lines.on("line", (line) => {
    let msg: any;
    try {
      msg = JSON.parse(line);
    } catch {
      return;
    }
    if (msg.method === undefined) {
      pending.get(msg.id)?.(msg.error ? null : msg.result);
      pending.delete(msg.id);
      return;
    }
    inflight++;
    handle(msg.method, msg.params ?? {})
      .then(
        (result) => msg.id !== undefined && write({ id: msg.id, result: result ?? null }),
        (e) => msg.id !== undefined && write({ id: msg.id, error: { code: e?.code ?? -32000, message: String(e?.message ?? e) } }),
      )
      .finally(() => --inflight === 0 && closed && process.exit(0));
  });
  // nyq closed stdin: finish what is running (e.g. session_end), then exit.
  lines.on("close", () => {
    closed = true;
    if (inflight === 0) process.exit(0);
  });
}
