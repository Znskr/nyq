//! End-to-end typed tasks: `nyq -p --schema …` against a fake
//! OpenAI-compatible server. The run prints the validated JSON and exits 0,
//! exits 2 with a JSON report when its budget runs out, and `--memo` answers a
//! repeated task without calling the model.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

/// Serve one canned SSE body per request, recording each request body. With
/// no replies the listener closes at once, so any request fails.
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

fn tool_call(id: &str, name: &str, args: Value) -> Value {
    delta(
        json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
        "function": {"name": name, "arguments": args.to_string()}}]}),
    )
}

fn text(t: &str) -> Value {
    delta(json!({"content": t}))
}

fn task_command(work: &Path, home: &Path, url: &str, prompt: &str, extra: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nyq"));
    cmd.args(["-p", "--yolo", "--sandbox", "off", "--provider", "openai"])
        .args(extra)
        .arg(prompt)
        .current_dir(work)
        .env("NYQ_HOME", home)
        .env("HOME", home)
        .env("NYQ_BASE_URL", url)
        .env("NYQ_MODEL", "fake-model")
        // A crate built by the verifier keeps its own target directory.
        .env("CARGO_TARGET_DIR", work.join("target"))
        .env_remove("NYQ_API_KEY");
    cmd
}

/// The text of the newest message of a chat-completions request.
fn last_message(req: &Value) -> String {
    let m = req["messages"].as_array().unwrap().last().unwrap();
    match &m["content"] {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn has_cargo() -> bool {
    Command::new("cargo")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn a_schema_task_on_a_cargo_crate_prints_the_validated_json() {
    if !has_cargo() {
        eprintln!("cargo not found; skipping");
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        work.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    std::fs::create_dir(work.path().join("src")).unwrap();
    std::fs::write(
        work.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { \"not a number\" }\n",
    )
    .unwrap();
    let schema = json!({
        "type": "object",
        "properties": {"fixed": {"type": "string"}, "files": {"type": "array", "items": {"type": "string"}}},
        "required": ["fixed", "files"]
    });
    let result = json!({"fixed": "add returns a + b", "files": ["src/lib.rs"]});
    let (url, seen) = fake_server(vec![
        // Submitting while the crate does not build is rejected…
        vec![tool_call("c1", "submit", result.clone())],
        // …so the model reads the file (edits need a prior read)…
        vec![tool_call("c2r", "read_file", json!({"path": "src/lib.rs"}))],
        // …fixes it and submits again.
        vec![tool_call(
            "c2",
            "write_file",
            json!({"path": "src/lib.rs", "content": "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"}),
        )],
        vec![tool_call("c3", "submit", result.clone())],
    ]);
    let out = task_command(
        work.path(),
        home.path(),
        &url,
        "make add compile",
        &[
            "--schema",
            &schema.to_string(),
            "--validate",
            "build",
            "--validate",
            "exists:/files",
        ],
    )
    .output()
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}\nstdout: {stdout}");
    let printed: Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    assert_eq!(printed, result, "stdout is the validated value alone");

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 4);
    let tools: Vec<&str> = seen[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert!(tools.contains(&"submit"), "{tools:?}");
    let first = last_message(&seen[0]);
    assert!(
        first.contains("YOUR CURRENT TASK: make add compile"),
        "{first}"
    );
    // The rejection carries the compiler's error back to the model.
    let second = seen[1]["messages"].to_string();
    assert!(second.contains("mismatched types"), "{second}");
    assert!(second.contains("rejected by the `build` check"), "{second}");
}

#[test]
fn a_task_that_never_submits_exits_2_with_a_report() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (url, seen) = fake_server(vec![
        vec![text("The answer is 42.")],
        vec![text("I said 42.")],
    ]);
    let out = task_command(
        work.path(),
        home.path(),
        &url,
        "what is six times seven?",
        &[
            "--schema",
            r#"{"type":"object","properties":{"answer":{"type":"integer"}},"required":["answer"]}"#,
            "--budget",
            "syntax=2,steps=5",
        ],
    )
    .output()
    .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {stderr}");
    assert!(out.stdout.is_empty(), "nothing validated, nothing printed");
    let report: Value = stderr
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str(l).ok())
        .unwrap_or_else(|| panic!("no JSON report in: {stderr}"));
    assert_eq!(report["status"], "exhausted");
    assert!(report["reason"]
        .as_str()
        .unwrap()
        .starts_with("syntax budget exhausted"));
    assert_eq!(report["budget"]["syntax_left"], 0);
    assert_eq!(report["goal"], "what is six times seven?");
    // The second request was the nudge.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(last_message(&seen[1]).contains("You must call `submit`"));
}

#[test]
fn memo_answers_a_repeated_task_without_the_model() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("input.txt"), "three apples").unwrap();
    let args = [
        "--schema",
        r#"{"type":"object","properties":{"count":{"type":"integer"}},"required":["count"]}"#,
        "--memo",
        "--memo-file",
        "input.txt",
    ];
    let prompt = "count the apples in input.txt";
    let (url, seen) = fake_server(vec![vec![tool_call("c1", "submit", json!({"count": 3}))]]);
    let first = task_command(work.path(), home.path(), &url, prompt, &args)
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&first.stdout).trim(),
        r#"{"count":3}"#
    );
    assert_eq!(seen.lock().unwrap().len(), 1);

    // No server answers now: a model call would fail the run.
    let (dead, seen) = fake_server(vec![]);
    let again = task_command(work.path(), home.path(), &dead, prompt, &args)
        .output()
        .unwrap();
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&again.stdout).trim(),
        r#"{"count":3}"#
    );
    assert!(seen.lock().unwrap().is_empty());

    // A changed input is a different task: the (dead) model is called.
    std::fs::write(work.path().join("input.txt"), "four apples").unwrap();
    let changed = task_command(work.path(), home.path(), &dead, prompt, &args)
        .output()
        .unwrap();
    assert_eq!(
        changed.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
}

#[test]
fn bad_task_flags_fail_with_status_1() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (url, _) = fake_server(vec![]);
    for extra in [
        &["--schema", "missing.json"][..],
        &["--validate", "nonsense"][..],
        &["--budget", "steps=lots"][..],
    ] {
        let out = task_command(work.path(), home.path(), &url, "x", extra)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(1),
            "{extra:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
