//! End-to-end: run the real binary in `--print` mode against a fake
//! OpenAI-compatible server and check the whole tool loop.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

/// Serve one canned SSE body per request, recording each request body.
fn fake_server(replies: Vec<Vec<Value>>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    thread::spawn(move || {
        for (stream, chunks) in listener.incoming().zip(replies) {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut len = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            log.lock()
                .unwrap()
                .push(serde_json::from_slice(&body).unwrap());
            let mut out = String::from(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            );
            for c in chunks {
                out.push_str(&format!("data: {c}\n\n"));
            }
            out.push_str("data: [DONE]\n\n");
            stream.write_all(out.as_bytes()).unwrap();
        }
    });
    (url, seen)
}

fn delta(d: Value) -> Value {
    json!({"choices": [{"index": 0, "delta": d}]})
}

#[test]
fn print_mode_runs_tools_and_answers() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("notes.txt"), "hello from the workspace\n").unwrap();

    let read_call = delta(
        json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
        "function": {"name": "read_file", "arguments": "{\"path\":\"notes.txt\"}"}}]}),
    );
    let write_call = delta(
        json!({"tool_calls": [{"index": 0, "id": "call_2", "type": "function",
        "function": {"name": "write_file", "arguments": "{\"path\":\"out.txt\",\"content\":\"done\\n\"}"}}]}),
    );
    let (url, seen) = fake_server(vec![
        vec![delta(json!({"content": "Reading."})), read_call],
        vec![write_call],
        vec![
            delta(json!({"content": "All "})),
            delta(json!({"content": "finished."})),
        ],
    ]);

    let out = Command::new(env!("CARGO_BIN_EXE_nyq"))
        .args(["--print", "--yolo", "--sandbox", "off", "copy the notes"])
        .current_dir(work.path())
        .env("NYQ_HOME", home.path()).env("HOME", home.path())
        .env("NYQ_BASE_URL", &url)
        .env("NYQ_MODEL", "fake-model")
        .env_remove("NYQ_API_KEY")
        .env_remove("NYQ_PROVIDER")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("All finished."), "stdout: {stdout}");
    assert_eq!(
        std::fs::read_to_string(work.path().join("out.txt")).unwrap(),
        "done\n"
    );

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0]["model"], "fake-model");
    // The second request carries the read_file result back to the model.
    let second = seen[1]["messages"].to_string();
    assert!(second.contains("hello from the workspace"), "{second}");
    assert!(second.contains("call_1"));

    // The conversation was saved and is listed for its directory.
    let sessions = |dir: &std::path::Path, args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_nyq"))
            .arg("sessions")
            .args(args)
            .current_dir(dir)
            .env("NYQ_HOME", home.path())
            .env("HOME", home.path())
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert!(sessions(work.path(), &[]).contains("copy the notes"));
    let elsewhere = tempfile::tempdir().unwrap();
    assert!(sessions(elsewhere.path(), &[]).contains("No saved conversations in this directory"));
    assert!(sessions(elsewhere.path(), &["--all"]).contains("copy the notes"));
    let listed: Value = serde_json::from_str(&sessions(work.path(), &["--json", "--limit", "1"])).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["title"], "copy the notes");
    // `nyq session ID` is short for `nyq session show ID`.
    let id = listed[0]["id"].as_str().unwrap();
    let show = Command::new(env!("CARGO_BIN_EXE_nyq"))
        .args(["session", &id[..8], "--json"])
        .current_dir(work.path())
        .env("NYQ_HOME", home.path())
        .env("HOME", home.path())
        .output()
        .unwrap();
    let shown: Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(shown["id"], id);
    assert!(shown["items"].as_array().unwrap().len() >= 4);
}

#[test]
fn print_mode_denies_mutations_without_yolo() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let write_call = delta(
        json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
        "function": {"name": "write_file", "arguments": "{\"path\":\"x.txt\",\"content\":\"x\"}"}}]}),
    );
    let (url, seen) = fake_server(vec![
        vec![write_call],
        vec![delta(json!({"content": "Okay."}))],
    ]);

    let out = Command::new(env!("CARGO_BIN_EXE_nyq"))
        .args(["-p", "--sandbox", "off", "write x"])
        .current_dir(work.path())
        .env("NYQ_HOME", home.path()).env("HOME", home.path())
        .env("NYQ_BASE_URL", &url)
        .env("NYQ_MODEL", "fake-model")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!work.path().join("x.txt").exists());
    assert!(seen.lock().unwrap()[1]["messages"]
        .to_string()
        .contains("denied"));
}

fn print_command(work: &std::path::Path, home: &std::path::Path, url: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nyq"));
    cmd.args([
        "--print",
        "--yolo",
        "--sandbox",
        "off",
        "--provider",
        "openai",
    ])
    .current_dir(work)
    .env("NYQ_HOME", home).env("HOME", home)
    .env("NYQ_BASE_URL", url)
    .env("NYQ_MODEL", "fake-model")
    .env("NYQ_CONTEXT_WINDOW", "1000")
    .env_remove("NYQ_API_KEY");
    cmd
}

fn tool_call(name: &str, args: Value) -> Value {
    delta(
        json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
        "function": {"name": name, "arguments": args.to_string()}}]}),
    )
}

fn assert_success(out: &std::process::Output) {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn session_items(home: &std::path::Path) -> Vec<Value> {
    let path = std::fs::read_dir(home.join("sessions"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .unwrap();
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn auto_compaction_and_resume_discard_earlier_tool_output() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("notes.txt"), "earlier unique tool output").unwrap();
    let summary = "Read notes.txt; continue the requested work.";
    let (url, seen) = fake_server(vec![
        vec![
            tool_call("read_file", json!({"path": "notes.txt"})),
            json!({"choices":[],"usage":{"prompt_tokens":950,"completion_tokens":5}}),
        ],
        vec![
            delta(json!({"content": summary})),
            json!({"choices":[],"usage":{"prompt_tokens":970,"completion_tokens":12}}),
        ],
        // An empty final response keeps the saved transcript at the summary,
        // so resume must send exactly the summary and the new user message.
        vec![],
        vec![delta(json!({"content": "Continued."}))],
    ]);
    assert_success(
        &print_command(work.path(), home.path(), &url)
            .arg("read the notes")
            .output()
            .unwrap(),
    );
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        let compact = &seen[1];
        assert!(compact.get("tools").is_none());
        let messages = compact["messages"].as_array().unwrap();
        assert_eq!(messages[messages.len() - 2]["role"], "tool");
        assert_eq!(messages[messages.len() - 2]["tool_call_id"], "call_1");
        let last = messages.last().unwrap();
        assert_eq!(last["role"], "user");
        let prompt = last["content"].as_str().unwrap();
        assert!(prompt.starts_with("Summarize this conversation so far"));
        assert!(prompt.ends_with("Do not call tools."));
        let following = seen[2]["messages"].as_array().unwrap();
        assert_eq!(following.len(), 2);
        assert_eq!(following[1]["role"], "user");
        assert!(following[1]["content"].as_str().unwrap().ends_with(summary));
        assert!(!seen[2]["messages"]
            .to_string()
            .contains("earlier unique tool output"));
    }
    assert!(session_items(home.path())
        .iter()
        .any(|item| item["type"] == "compaction" && item["summary"] == summary));
    assert_success(
        &print_command(work.path(), home.path(), &url)
            .args(["-c", "next"])
            .output()
            .unwrap(),
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 4);
    let resumed = seen[3]["messages"].as_array().unwrap();
    assert_eq!(resumed.len(), 3);
    assert!(resumed[1]["content"].as_str().unwrap().ends_with(summary));
    assert_eq!(resumed[2]["role"], "user");
    assert_eq!(without_note(&resumed[2]["content"]), "next");
}

/// A message's text without the per-request session note nyq appends to the
/// newest message.
fn without_note(text: &Value) -> &str {
    let text = text.as_str().unwrap();
    text.split("\n\n[Session state]").next().unwrap_or(text)
}

// A complete 1x1 PNG, rather than just the image magic bytes.
const PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aPa0AAAAASUVORK5CYII=";

fn write_png(work: &std::path::Path) {
    use base64::Engine;
    std::fs::write(
        work.join("shot.png"),
        base64::engine::general_purpose::STANDARD
            .decode(PNG_BASE64)
            .unwrap(),
    )
    .unwrap();
}

fn assert_image(message: &Value) {
    assert_eq!(message["role"], "user");
    let parts = message["content"].as_array().unwrap();
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(
        parts[1],
        json!({"type":"image_url","image_url":{"url":format!("data:image/png;base64,{PNG_BASE64}")}})
    );
}

#[test]
fn prompt_attaches_workspace_image() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write_png(work.path());
    let (url, seen) = fake_server(vec![vec![delta(json!({"content":"A pixel."}))]]);
    assert_success(
        &print_command(work.path(), home.path(), &url)
            .arg("describe @shot.png")
            .output()
            .unwrap(),
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_image(&seen[0]["messages"][1]);
    assert_eq!(
        without_note(&seen[0]["messages"][1]["content"][0]["text"]),
        "describe @shot.png"
    );
}

#[test]
fn view_image_sends_result_then_user_image() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write_png(work.path());
    let (url, seen) = fake_server(vec![
        vec![tool_call("view_image", json!({"path":"shot.png"}))],
        vec![delta(json!({"content":"A pixel."}))],
    ]);
    assert_success(
        &print_command(work.path(), home.path(), &url)
            .arg("inspect the screenshot")
            .output()
            .unwrap(),
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let messages = seen[1]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[2]["tool_calls"][0]["id"], "call_1");
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_1");
    assert!(messages[3]["content"]
        .as_str()
        .unwrap()
        .contains("Loaded shot.png"));
    assert_image(&messages[4]);
}

#[test]
fn oversized_compaction_warns_and_continues() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (url, seen) = fake_server(vec![
        vec![
            tool_call("list_dir", json!({"path":"."})),
            json!({"choices":[],"usage":{"prompt_tokens":950,"completion_tokens":5}}),
        ],
        vec![
            delta(json!({"content":"A summary."})),
            json!({"choices":[],"usage":{"prompt_tokens":970,"completion_tokens":1000}}),
        ],
        vec![delta(json!({"content":"Continued."}))],
    ]);
    let out = print_command(work.path(), home.path(), &url)
        .arg("look around")
        .output()
        .unwrap();
    assert_success(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Automatic compaction is off"), "{stderr}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("Continued."));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    let compact = seen[1]["messages"].to_string();
    assert!(compact.contains("Summarize this conversation"), "{compact}");
    let next = seen[2]["messages"].to_string();
    assert!(!next.contains("Summarize this conversation"), "{next}");
}

#[test]
fn usage_is_logged_and_reported_without_exposing_auth() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("models.json"), r#"{"openai":{"models":{"priced":{"cost":{"input":2,"output":8,"cache_read":0.5}}}}}"#).unwrap();
    let (url, _) = fake_server(vec![vec![
        delta(json!({"content":"Done."})),
        json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":40}}}),
    ]]);
    let mut cmd = print_command(work.path(), home.path(), &url);
    let out = cmd.env("NYQ_MODEL", "priced").arg("hello").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let log = std::fs::read_to_string(home.path().join("usage.jsonl")).unwrap();
    assert_eq!(log.lines().count(), 1);
    let r: Value = serde_json::from_str(log.trim()).unwrap();
    assert_eq!((r["input"].as_u64(), r["cached"].as_u64(), r["output"].as_u64()), (Some(100),Some(40),Some(20)));
    assert_eq!(r["provider"], "openai");
    assert_eq!(r["model"], "priced");
    assert!(!r["session"].as_str().unwrap().is_empty());
    assert!((r["cost_usd"].as_f64().unwrap() - 0.0003).abs() < 1e-12);
    let report = Command::new(env!("CARGO_BIN_EXE_nyq")).args(["usage", "--period", "24h"])
        .env("HOME", home.path()).env("NYQ_HOME", home.path()).output().unwrap();
    assert!(report.status.success());
    let text = String::from_utf8_lossy(&report.stderr);
    assert!(text.contains("openai/priced: 1 requests"), "{text}");
    assert!(text.contains("$0.000300"), "{text}");
    let out = Command::new(env!("CARGO_BIN_EXE_nyq")).args(["status", "--json", "--provider", "openai"])
        .env("HOME", home.path()).env("NYQ_HOME", home.path()).env("NYQ_BASE_URL", &url)
        .env("NYQ_MODEL", "priced").env("NYQ_API_KEY", "must-never-appear").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("must-never-appear"));
    let status: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(status["model"], "priced");
    assert!(status["auth_sources"].as_array().unwrap().contains(&json!("NYQ_API_KEY")));
}

#[test]
fn usage_status_and_doctor_commands_work_without_model_requests() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let record = json!({"ts":chrono::Utc::now(),"session":"s","provider":"openai","model":"priced","input":100,"cached":40,"output":20,"cost_usd":0.0003});
    std::fs::write(home.path().join("usage.jsonl"), format!("{record}\n")).unwrap();
    let command = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_nyq"));
        cmd.current_dir(work.path()).env("HOME", home.path()).env("NYQ_HOME", home.path())
            .env_remove("NYQ_PROVIDER").env_remove("NYQ_MODEL").env_remove("NYQ_BASE_URL").env_remove("NYQ_API_KEY")
            .env("NYQ_CODEX_AUTH", home.path().join("missing.json"));
        cmd
    };
    let report = command().args(["usage", "--period", "24h"]).output().unwrap();
    assert!(report.status.success());
    assert!(String::from_utf8_lossy(&report.stderr).contains("openai/priced: 1 requests"));
    let status = command().args(["status", "--json", "--provider", "openai"])
        .env("NYQ_BASE_URL", "http://127.0.0.1:1/v1").env("NYQ_MODEL", "priced")
        .env("NYQ_API_KEY", "must-never-appear").output().unwrap();
    assert!(status.status.success());
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(!text.contains("must-never-appear"));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["model"], "priced");
    assert!(v["auth_sources"].as_array().unwrap().contains(&json!("NYQ_API_KEY")));
    let doctor = command().args(["doctor", "--sandbox", "off"]).output().unwrap();
    assert!(!doctor.status.success());
    let text = String::from_utf8_lossy(&doctor.stderr);
    assert!(text.contains("✗ auth:"));
    assert!(text.contains("run nyq login"));
    assert!(text.contains("✓ profile:"));
}

#[test]
#[cfg(unix)]
fn print_mode_runs_user_hooks_but_not_untrusted_project_hooks() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let log = home.path().join("events.log");
    let l = log.display();
    let hooks = json!({"hooks": [
        {"event": "SessionStart", "command": format!("echo start >> {l}")},
        {"event": "UserPromptSubmit", "command": "echo 'HOOK CONTEXT: branch main'"},
        {"event": "PreToolUse", "matcher": "write_*", "command": "echo '{\"decision\":\"block\",\"reason\":\"read-only today\"}'"},
        {"event": "PostToolUse", "command": format!("echo post >> {l}")},
        {"event": "Stop", "command": format!("echo stop >> {l}; echo stop-stderr >&2")},
    ]});
    std::fs::write(home.path().join("hooks.json"), hooks.to_string()).unwrap();
    std::fs::create_dir_all(work.path().join(".nyq")).unwrap();
    let project = json!({"hooks": [{"event": "SessionStart", "command": format!("echo project >> {l}")}]});
    std::fs::write(work.path().join(".nyq/hooks.json"), project.to_string()).unwrap();

    let (url, seen) = fake_server(vec![
        vec![tool_call("write_file", json!({"path": "x.txt", "content": "x"}))],
        vec![delta(json!({"content": "Okay."}))],
    ]);
    let out = print_command(work.path(), home.path(), &url).arg("write x").output().unwrap();
    assert_success(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not trusted"), "{stderr}");
    assert!(stderr.contains("stop-stderr"), "{stderr}");
    assert!(!work.path().join("x.txt").exists());
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "start\nstop\n");
    let seen = seen.lock().unwrap();
    let first = seen[0]["messages"].to_string();
    assert!(first.contains("HOOK CONTEXT: branch main"), "{first}");
    let second = seen[1]["messages"].to_string();
    assert!(second.contains("read-only today"), "{second}");
}

#[test]
fn print_mode_ignores_a_saved_yolo_mode() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("settings.toml"), "permission_mode = \"yolo\"\n").unwrap();
    let write_call = delta(json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
        "function": {"name": "write_file", "arguments": "{\"path\":\"x.txt\",\"content\":\"x\"}"}}]}));
    let (url, _seen) = fake_server(vec![vec![write_call], vec![delta(json!({"content": "Okay."}))]]);

    let out = Command::new(env!("CARGO_BIN_EXE_nyq"))
        .args(["-p", "--sandbox", "off", "--provider", "openai", "write x"])
        .current_dir(work.path())
        .env("NYQ_HOME", home.path())
        .env("HOME", home.path())
        .env("NYQ_BASE_URL", &url)
        .env("NYQ_MODEL", "fake-model")
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("saved yolo permission mode does not apply"));
    assert!(!work.path().join("x.txt").exists(), "a saved yolo must not let print mode write files");
}

#[test]
fn acp_ignores_saved_yolo_unless_explicit() {
    for (flags, env_mode, expected) in [
        (vec![], None, "ask"),
        (vec!["--yolo"], None, "yolo"),
        (vec!["--permission-mode", "yolo"], None, "yolo"),
        (vec![], Some("yolo"), "yolo"),
    ] {
        let work = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("settings.toml"), "permission_mode = \"yolo\"\n").unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_nyq"));
        cmd.args(["acp", "--sandbox", "off", "--provider", "openai"]).args(flags)
            .current_dir(work.path()).env("NYQ_HOME", home.path()).env("HOME", home.path())
            .env("NYQ_BASE_URL", "http://127.0.0.1:1/v1").env("NYQ_MODEL", "fake-model")
            .env_remove("NYQ_PERMISSION_MODE");
        if let Some(mode) = env_mode {
            cmd.env("NYQ_PERMISSION_MODE", mode);
        }
        let mut child = cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null())
            .spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let value: Value = serde_json::from_str(&line.unwrap()).unwrap();
                if value["id"].is_number() {
                    let _ = tx.send(value);
                }
            }
        });
        writeln!(input, "{}", json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":1,"clientCapabilities":{}}})).unwrap();
        let init = rx.recv_timeout(std::time::Duration::from_secs(20));
        writeln!(input, "{}", json!({"jsonrpc":"2.0", "id":2, "method":"session/new", "params":{"cwd":work.path(),"mcpServers":[]}})).unwrap();
        let session = rx.recv_timeout(std::time::Duration::from_secs(20));
        let _ = child.kill();
        child.wait().unwrap();
        reader.join().unwrap();
        assert!(init.unwrap().get("result").is_some());
        let session = session.unwrap();
        assert_eq!(session["result"]["modes"]["currentModeId"], expected, "{session}");
    }
}

/// `nyq ask` against `url` from `work`, without the --yolo of `print_command`.
fn ask_command(work: &std::path::Path, home: &std::path::Path, url: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nyq"));
    cmd.args(["ask", "--sandbox", "off", "--provider", "openai"])
        .current_dir(work)
        .env("NYQ_HOME", home)
        .env("HOME", home)
        .env("NYQ_BASE_URL", url)
        .env("NYQ_MODEL", "fake-model")
        .env_remove("NYQ_API_KEY")
        .env_remove("NYQ_PERMISSION_MODE")
        .env_remove("NYQ_MAX_AGENT_STEPS");
    cmd
}

#[test]
fn ask_json_reports_answer_tools_and_session() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("notes.txt"), "hello\n").unwrap();
    write_png(work.path());
    let (url, seen) = fake_server(vec![
        vec![
            delta(json!({"content": "Let me look."})),
            tool_call("read_file", json!({"path": "notes.txt"})),
        ],
        vec![delta(json!({"content": "It says hello."}))],
    ]);
    let out = ask_command(work.path(), home.path(), &url)
        .args(["--json", "--system", "You are a terse test bot.", "--image", "shot.png", "what is in notes"])
        .output()
        .unwrap();
    assert_success(&out);
    // Only the JSON object is on stdout.
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["exit_code"], 0);
    assert_eq!(v["model"], "fake-model");
    assert_eq!(v["steps"], 2);
    assert_eq!(v["final_output"], "It says hello.");
    assert!(v["output"].as_str().unwrap().contains("Let me look."));
    assert_eq!(v["tool_calls"][0]["name"], "read_file");
    assert_eq!(v["tool_calls"][0]["status"], "ok");
    assert!(v["error"].is_null());
    let id = v["session_id"].as_str().unwrap();
    assert!(home.path().join("sessions").join(format!("{id}.jsonl")).exists());

    let seen = seen.lock().unwrap();
    let system = seen[0]["messages"][0]["content"].as_str().unwrap();
    assert!(system.starts_with("You are a terse test bot."), "{system}");
    assert!(!system.contains("You are nyq"));
    assert!(system.contains("Working directory"), "context still applies: {system}");
    assert_image(&seen[0]["messages"][1]);
}

#[test]
fn ask_json_reports_startup_errors_and_no_save_keeps_nothing() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let out = ask_command(work.path(), home.path(), "http://127.0.0.1:1/v1")
        .args(["--json", "--context-limit", "bogus=1", "hi"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["exit_code"], 1);
    assert!(v["error"].as_str().unwrap().contains("unknown context limit"), "{v}");

    let (url, _seen) = fake_server(vec![vec![delta(json!({"content": "Hi."}))]]);
    let out = ask_command(work.path(), home.path(), &url)
        .args(["--no-save", "--quiet", "hi"])
        .output()
        .unwrap();
    assert_success(&out);
    assert!(out.stdout.is_empty(), "--quiet prints nothing");
    let saved = std::fs::read_dir(home.path().join("sessions"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(saved, 0, "--no-save must not create a session");
    let out = ask_command(work.path(), home.path(), &url)
        .args(["--no-save", "-c", "hi"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn env_sets_permission_mode_and_step_limit() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (url, _seen) = fake_server(vec![
        vec![tool_call("write_file", json!({"path": "x.txt", "content": "x"}))],
        vec![delta(json!({"content": "Done."}))],
    ]);
    let out = ask_command(work.path(), home.path(), &url)
        .env("NYQ_PERMISSION_MODE", "yolo")
        .arg("write x")
        .output()
        .unwrap();
    assert_success(&out);
    assert!(work.path().join("x.txt").exists(), "NYQ_PERMISSION_MODE=yolo allows writes");

    let (url, _seen) = fake_server(vec![vec![tool_call("read_file", json!({"path": "x.txt"}))]]);
    let out = ask_command(work.path(), home.path(), &url)
        .env("NYQ_MAX_AGENT_STEPS", "1")
        .args(["--json", "read x"])
        .output()
        .unwrap();
    // Reaching the limit ends the run with a notice, not an error.
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["notices"][0].as_str().unwrap().contains("Stopped after 1 model steps"), "{v}");
    assert_eq!(v["tool_calls"][0]["status"], "ok");
}

/// A model endpoint that accepts connections and never answers.
fn silent_server() -> (String, std::sync::mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            held.push(stream);
            let _ = tx.send(());
        }
    });
    (url, rx)
}

#[test]
#[cfg(unix)]
fn signals_interrupt_print_mode_with_conventional_exit_codes() {
    for (signal, code) in [("-INT", 130), ("-TERM", 143)] {
        let work = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let (url, connected) = silent_server();
        let child = ask_command(work.path(), home.path(), &url)
            .args(["--json", "wait"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        connected.recv_timeout(std::time::Duration::from_secs(20)).unwrap();
        let status = Command::new("kill")
            .args([signal, &child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(code), "stderr: {}", String::from_utf8_lossy(&out.stderr));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["exit_code"], code);
    }
}

#[test]
#[cfg(unix)]
fn signals_interrupt_print_mode_while_stdin_is_open() {
    for (signal, code) in [("-INT", 130), ("-TERM", 143)] {
        let work = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut child = ask_command(work.path(), home.path(), "http://127.0.0.1:1/v1")
            .arg("--json")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn().unwrap();
        // Hold the pipe open after partial input: EOF must not be needed to exit.
        let mut input = child.stdin.take().unwrap();
        // More than a pipe buffer makes this wait until nyq starts reading.
        input.write_all(&vec![b'x'; 1024 * 1024]).unwrap();
        thread::sleep(std::time::Duration::from_millis(50));
        assert!(Command::new("kill").args([signal, &child.id().to_string()]).status().unwrap().success());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
            thread::sleep(std::time::Duration::from_millis(10));
        }
        let exited = child.try_wait().unwrap().is_some();
        if !exited {
            let _ = child.kill();
        }
        let out = child.wait_with_output().unwrap();
        drop(input);
        assert!(exited, "did not exit while stdin was open");
        assert_eq!(out.status.code(), Some(code), "stderr: {}", String::from_utf8_lossy(&out.stderr));
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["exit_code"], code);
        assert_eq!(value["steps"], 0);
    }
}

#[test]
fn print_mode_reads_the_complete_piped_prompt() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (url, seen) = fake_server(vec![vec![delta(json!({"content": "Read it."}))]]);
    let mut child = ask_command(work.path(), home.path(), &url)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"hello\nfrom stdin").unwrap();
    assert_success(&child.wait_with_output().unwrap());
    let seen = seen.lock().unwrap();
    let messages = seen[0]["messages"].as_array().unwrap();
    let user = messages.iter().find(|m| m["role"] == "user").unwrap();
    assert!(user["content"].to_string().contains("hello\\nfrom stdin"), "{user}");
}

#[test]
fn continue_recovery_resumes_the_cut_off_answer() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (url, seen) = fake_server(vec![
        vec![delta(json!({"content": "Part one"}))],
        vec![delta(json!({"content": " and part two."}))],
    ]);
    assert_success(&ask_command(work.path(), home.path(), &url).arg("write it").output().unwrap());
    let out = ask_command(work.path(), home.path(), &url)
        .args(["-c", "--continue-recovery"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_success(&out);
    assert!(String::from_utf8_lossy(&out.stdout).contains("part two"));
    let seen = seen.lock().unwrap();
    let system = seen[1]["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("Continue from where you stopped"), "{system}");
    // No new user message was added.
    let users = seen[1]["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "user").count();
    assert_eq!(users, 1);
}

#[test]
fn file_and_search_results_redact_password_assignments() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let secret = "audit_password_12345";
    std::fs::write(work.path().join("config.env"), format!("DATABASE_PASSWORD={secret}\n")).unwrap();
    let calls = [("read_file", json!({"path":"config.env"})), ("grep", json!({"pattern":"PASSWORD"}))];
    let mut replies: Vec<Vec<Value>> = calls.iter().enumerate().map(|(i, (name, args))| {
        vec![delta(json!({"tool_calls":[{"index":0,"id":format!("call_{i}"),"type":"function",
            "function":{"name":name,"arguments":args.to_string()}}]}))]
    }).collect();
    replies.push(vec![delta(json!({"content":"Done."}))]);
    let (url, seen) = fake_server(replies);
    let out = Command::new(env!("CARGO_BIN_EXE_nyq"))
        .args(["-p", "--provider", "openai", "--sandbox", "off", "inspect config"])
        .current_dir(work.path())
        .env("NYQ_HOME", home.path()).env("HOME", home.path())
        .env("NYQ_BASE_URL", url).env("NYQ_MODEL", "fake-model")
        .output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    let sent = serde_json::to_string(&*seen).unwrap();
    assert!(!sent.contains(secret));
    assert!(sent.contains("DATABASE_PASSWORD=[redacted]"));
    for entry in std::fs::read_dir(home.path().join("sessions")).unwrap().flatten() {
        if entry.path().extension().is_some_and(|e| e == "jsonl") {
            assert!(!std::fs::read_to_string(entry.path()).unwrap().contains(secret));
        }
    }
}
