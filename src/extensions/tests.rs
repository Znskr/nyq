use super::*;

/// A small extension exercising every part of the protocol.
const FIXTURE: &str = r#"
import json, sys

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    method, params, mid = msg.get("method"), msg.get("params") or {}, msg.get("id")
    result = None
    if method == "initialize":
        result = {
            "tools": [
                {"name": "echo", "description": "Echo", "read_only": True,
                 "input_schema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
                {"name": "crash", "description": "Exit"},
            ],
            "commands": [{"name": "hi", "description": "Greet", "args_hint": "[name]"},
                         {"name": "send", "description": "Queue a message"},
                         {"name": "help", "description": "Taken"}],
            "events": ["user_prompt", "tool_call", "tool_result", "before_compact", "session_start"],
            "status": "fixture " + params["mode"],
        }
    elif method == "tool/call":
        if params["name"] == "crash":
            sys.exit(3)
        result = {"content": [{"type": "text", "text": "echo: " + params["input"]["text"]}]}
    elif method == "command/run":
        if params["name"] == "send":
            send({"jsonrpc": "2.0", "method": "session/send", "params": {"text": "queued " + params["args"], "queue": "steer"}})
            send({"jsonrpc": "2.0", "method": "status/set", "params": {"text": "sent"}})
            result = {}
        else:
            result = {"message": "hello " + params["args"], "prompt": "say hi to " + params["args"]}
    elif method == "event/user_prompt":
        p = params["prompt"]
        if "forbidden" in p:
            result = {"block": "no forbidden words"}
        elif p.startswith("rewrite:"):
            result = {"prompt": p[len("rewrite:"):].strip(), "context": "ctx"}
        else:
            result = {}
    elif method == "event/tool_call":
        inp = params["input"]
        if "rm -rf" in json.dumps(inp):
            result = {"block": "rm -rf is not allowed"}
        elif inp.get("text") == "rewrite me":
            result = {"input": {"text": "rewritten"}}
        else:
            result = {}
    elif method == "event/tool_result":
        if params["content"] == "slow":
            import time; time.sleep(2)
        result = {"content": params["content"].upper()}
    elif method == "event/before_compact":
        result = {"instructions": "keep the TODOs"}
    elif mid is None:
        continue
    else:
        result = {}
    if mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "result": result})
"#;

fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| {
        std::process::Command::new(p).arg("--version").output().is_ok_and(|o| o.status.success())
    })
}

fn write_ext(root: &Path, name: &str, extra: &str) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ext.py"), FIXTURE).unwrap();
    let py = python().unwrap_or("python3");
    std::fs::write(
        dir.join(MANIFEST),
        format!(r#"{{"name": "{name}", "version": "0.1", "command": ["{py}", "ext.py"], "event_timeout_ms": 500{extra}}}"#),
    )
    .unwrap();
    dir
}

async fn ready(home: &Path, cwd: &Path) -> (Extensions, Registry) {
    let (ext, warnings) = Extensions::load(home, cwd, Mode::Print);
    assert!(warnings.is_empty(), "{warnings:?}");
    ext.reserve(["help".to_string()]);
    let mut tools = Registry::default();
    ext.attach(&mut tools);
    ext.start();
    ext.wait_ready(Duration::from_secs(20)).await;
    tools.refresh();
    (ext, tools)
}

async fn call(tools: &Registry, name: &str, args: Value) -> ToolOutput {
    tools.get(name).expect(name).run(args, &ToolCtx::default()).await
}

#[tokio::test]
async fn an_extension_registers_tools_commands_and_handles_events() {
    if python().is_none() {
        eprintln!("skipped: no python");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_ext(&user_dir(home.path()), "probe", "");
    let (ext, mut tools) = ready(home.path(), cwd.path()).await;
    let lines = ext.lines().join("\n");
    assert!(lines.contains("probe 0.1 · user · running"), "{lines}");

    // Tools: named ext__<ext>__<tool>; read_only decides approval; schema checked.
    let echo = tools.get("ext__probe__echo").unwrap();
    assert!(!echo.needs_approval(&json!({})));
    assert!(tools.get("ext__probe__crash").unwrap().needs_approval(&json!({})));
    assert_eq!(call(&tools, "ext__probe__echo", json!({"text": "hi"})).await.content, "echo: hi");
    assert!(call(&tools, "ext__probe__echo", json!({})).await.is_error);

    // Commands: reserved names are skipped.
    assert_eq!(ext.commands().iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>(), ["/hi", "/send"]);
    assert!(ext.has_command("/hi") && !ext.has_command("/help"));
    let r = ext.run_command("/hi", "bob", &CancellationToken::new()).await.unwrap();
    assert_eq!(r, CommandResult { message: Some("hello bob".into()), prompt: Some("say hi to bob".into()) });

    // Status and session/send.
    assert_eq!(ext.statuses(), ["fixture print"]);
    ext.run_command("/send", "x", &CancellationToken::new()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), ext.queued()).await.unwrap();
    for _ in 0..100 {
        if ext.statuses() == ["sent"] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(ext.statuses(), ["sent"]);
    assert_eq!(ext.take_queued(false), Vec::<String>::new());
    assert_eq!(ext.take_queued(true), ["queued x"]);

    // Events: block, rewrite, context.
    assert!(ext.user_prompt("a forbidden word").await.blocked.unwrap().contains("no forbidden words"));
    let edit = ext.user_prompt("rewrite: tidy up").await;
    assert_eq!(edit.prompt.as_deref(), Some("tidy up"));
    assert_eq!(edit.context, ["[extension probe added this context]\nctx"]);
    assert_eq!(ext.user_prompt("plain").await, PromptEdit::default());
    let blocked = ext.tool_call("bash", &json!({"command": "rm -rf /"})).await.unwrap_err();
    assert!(blocked.contains("rm -rf is not allowed"), "{blocked}");
    assert_eq!(ext.tool_call("x", &json!({"text": "rewrite me"})).await, Ok(Some(json!({"text": "rewritten"}))));
    assert_eq!(ext.tool_call("x", &json!({"text": "fine"})).await, Ok(None));
    let mut out = ToolOutput::ok("result");
    ext.tool_result("x", &json!({}), &mut out).await;
    assert_eq!(out.content, "RESULT");
    assert_eq!(ext.before_compact().await, ["keep the TODOs"]);
    // A handler that misses its timeout changes nothing.
    let mut slow = ToolOutput::ok("slow");
    ext.tool_result("x", &json!({}), &mut slow).await;
    assert_eq!(slow.content, "slow");
    let log = std::fs::read_to_string(log_path(home.path(), "probe")).unwrap();
    assert!(log.contains("event/tool_result skipped"), "{log}");

    // A crash restarts the extension once; a second crash is reported.
    let out = call(&tools, "ext__probe__crash", json!({})).await;
    assert!(out.is_error, "{}", out.content);
    // A call right after the crash waits for the restart.
    assert_eq!(call(&tools, "ext__probe__echo", json!({"text": "again"})).await.content, "echo: again");
    tools.refresh();
    assert!(tools.get("ext__probe__echo").is_some());
    call(&tools, "ext__probe__crash", json!({})).await;
    for _ in 0..200 {
        if ext.lines().join("\n").contains("failed: exited") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let lines = ext.lines().join("\n");
    assert!(lines.contains("failed: exited"), "{lines}");
    tools.refresh();
    assert!(tools.get("ext__probe__echo").is_none());
    ext.shutdown().await;
}

#[test]
fn project_extensions_need_trust_by_content_hash() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_ext(&user_dir(home.path()), "mine", "");
    let dir = write_ext(&project_dir(cwd.path()), "theirs", "");
    // Same name as a user extension: skipped.
    write_ext(&project_dir(cwd.path()), "zz", "");
    std::fs::write(dir.parent().unwrap().join("zz").join(MANIFEST), r#"{"name": "mine", "command": ["x"]}"#).unwrap();

    let (found, warnings) = discover(home.path(), cwd.path());
    let names: Vec<(&str, bool, bool)> = found.iter().map(|f| (f.manifest.name.as_str(), f.project, f.trusted)).collect();
    assert_eq!(names, [("mine", false, true), ("theirs", true, false)]);
    assert!(warnings.iter().any(|w| w.contains("not trusted")), "{warnings:?}");
    assert!(warnings.iter().any(|w| w.contains("already loaded")), "{warnings:?}");

    assert_eq!(trust(home.path(), &found).unwrap(), ["theirs"]);
    let (found, warnings) = discover(home.path(), cwd.path());
    assert!(found[1].trusted, "{warnings:?}");

    // Any change to its files (not just the manifest) needs trust again.
    std::fs::write(dir.join("ext.py"), "print('changed')").unwrap();
    let (changed, _) = discover(home.path(), cwd.path());
    assert!(!changed[1].trusted);
    // Dependency directories are not hashed.
    let before = dir_hash(&dir);
    std::fs::create_dir_all(dir.join("node_modules/x")).unwrap();
    std::fs::write(dir.join("node_modules/x/i.js"), "1").unwrap();
    assert_eq!(dir_hash(&dir), before);

    // Trusting what was listed refuses a directory that changed since.
    std::fs::write(dir.join("ext.py"), "print('changed again')").unwrap();
    assert!(trust(home.path(), &changed).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn cli_install_copies_and_runs_the_install_command() {
    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    write_ext(src.path(), "a", r#", "install": ["sh", "-c", "touch installed"]"#);
    write_ext(src.path(), "b", "");
    std::fs::create_dir_all(src.path().join("a/.git")).unwrap();
    let names = cli::install(home.path(), src.path(), &src.path().display().to_string(), true).await.unwrap();
    assert_eq!(names, ["a", "b"]);
    let a = user_dir(home.path()).join("a");
    assert!(a.join("ext.py").is_file() && a.join("installed").is_file() && !a.join(".git").exists());
    // A relative path installs just that extension, replacing the old copy.
    assert_eq!(cli::install(home.path(), src.path(), "b", true).await.unwrap(), ["b"]);
    let (found, _) = discover(home.path(), src.path());
    assert_eq!(found.len(), 2);
}

#[test]
fn examples_parse_and_the_node_example_bundles_the_sdk() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/extensions");
    let (found, warnings) = discover(&root.join("none"), &root.join("none"));
    assert!(found.is_empty() && warnings.is_empty());
    let names: Vec<String> = scan(&root, &mut Vec::new()).into_iter().map(|(_, m)| m.name).collect();
    assert_eq!(names, ["git-status", "word-tools"]);
    let read = |p: &str| std::fs::read_to_string(root.join(p)).unwrap();
    assert_eq!(read("git-status/nyq-extension.mjs"), read("sdk/nyq-extension.mjs"), "copy sdk/nyq-extension.mjs into git-status/");
}
