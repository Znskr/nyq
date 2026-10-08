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

    // The conversation was saved and is listed.
    let list = Command::new(env!("CARGO_BIN_EXE_nyq"))
        .arg("sessions")
        .env("NYQ_HOME", home.path()).env("HOME", home.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&list.stdout).contains("copy the notes"));
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
    assert_eq!(resumed[2], json!({"role":"user","content":"next"}));
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
        seen[0]["messages"][1]["content"][0]["text"],
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
    std::fs::write(home.path().join("settings.json"), r#"{"permission_mode":"yolo"}"#).unwrap();
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
