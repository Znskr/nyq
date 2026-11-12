//! Release smoke test of the real binary, headless, against a scripted fake
//! OpenAI-compatible server: a typed `--schema` task on a temporary cargo
//! crate that edits with `multi_edit` while the background verifier (turned
//! on by the `ci` profile's `verify.required`) watches the build; then
//! `nyq export --redact` of that session; then `nyq --profile minimal
//! --dump-config`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
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

fn tool_call(id: &str, name: &str, args: Value) -> Vec<Value> {
    vec![json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
        "index": 0, "id": id, "type": "function",
        "function": {"name": name, "arguments": args.to_string()}
    }]}}]})]
}

fn nyq(work: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nyq"));
    cmd.current_dir(work)
        .env("NYQ_HOME", home)
        .env("HOME", home)
        .env("NYQ_MODEL", "fake-model")
        // A crate built by the verifier keeps its own target directory.
        .env("CARGO_TARGET_DIR", work.join("target"))
        .env_remove("NYQ_API_KEY")
        .env_remove("NYQ_PROFILE");
    cmd
}

/// The text of the newest message of a chat-completions request (where the
/// trailing note rides).
fn last_message(req: &Value) -> String {
    let m = req["messages"].as_array().unwrap().last().unwrap();
    match &m["content"] {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn tool_names(req: &Value) -> Vec<String> {
    req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_string())
        .collect()
}

fn has_cargo() -> bool {
    Command::new("cargo")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

const LIB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n\npub fn sub(a: i32, b: i32) -> i32 {\n    a + b\n}\n";

#[test]
fn release_smoke_typed_task_export_and_profile() {
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
    std::fs::write(work.path().join("src/lib.rs"), LIB).unwrap();
    // The `ci` profile requires verification; these user keys only make the
    // background verifier deterministic (no debounce, wait for each build).
    std::fs::write(
        home.path().join("settings.toml"),
        "[verify]\ndebounce_ms = 0\nwait_ms = 120000\n",
    )
    .unwrap();

    let schema = json!({
        "type": "object",
        "properties": {"fixed": {"type": "array", "items": {"type": "string"}}},
        "required": ["fixed"]
    });
    let result = json!({"fixed": ["add", "sub"]});
    let (url, seen) = fake_server(vec![
        // Edits need a prior read (the read-before-edit guard).
        tool_call("r1", "read_file", json!({"path": "src/lib.rs"})),
        // One atomic multi_edit swaps both operators, with a stray `;` that
        // breaks the build, and states its intent.
        tool_call(
            "e1",
            "multi_edit",
            json!({
                "path": "src/lib.rs",
                "edits": [
                    {"old_string": "    a - b\n}\n\npub fn sub", "new_string": "    a + b;\n}\n\npub fn sub"},
                    {"old_string": "-> i32 {\n    a + b\n}\n", "new_string": "-> i32 {\n    a - b\n}\n"}
                ],
                "intent": {"did": "swap the operators", "why": "add and sub are swapped", "next": "check the build"}
            }),
        ),
        // The verifier reported the broken build; fix it.
        tool_call(
            "e2",
            "multi_edit",
            json!({
                "path": "src/lib.rs",
                "edits": [{"old_string": "a + b;", "new_string": "a + b"}],
                "intent": {"did": "drop the stray semicolon", "why": "the build failed"}
            }),
        ),
        tool_call("s1", "submit", result.clone()),
    ]);
    let out = nyq(work.path(), home.path())
        .args([
            "--profile",
            "ci",
            "-p",
            "--sandbox",
            "off",
            "--provider",
            "openai",
            "--schema",
            &schema.to_string(),
            // A command check, not a build check: the background verifier
            // runs only because the `ci` profile requires verification.
            "--validate",
            "cmd:cargo build --offline --quiet",
            "fix add and sub in src/lib.rs",
        ])
        .env("NYQ_BASE_URL", &url)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}\nstdout: {stdout}");
    let printed: Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    assert_eq!(printed, result, "stdout is the validated value alone");
    assert_eq!(
        std::fs::read_to_string(work.path().join("src/lib.rs")).unwrap(),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n"
    );

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 4, "read, edit, fix, submit");
    // Typed task: `submit`, and the intent field on the mutating tools.
    let tools = tool_names(&seen[0]);
    assert!(tools.contains(&"submit".to_string()), "{tools:?}");
    let multi = seen[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "multi_edit")
        .unwrap();
    assert!(multi["function"]["parameters"]["properties"]["intent"].is_object());
    let read = seen[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "read_file")
        .unwrap();
    assert!(read["function"]["parameters"]["properties"]
        .get("intent")
        .is_none());
    let first = last_message(&seen[0]);
    assert!(
        first.contains("YOUR CURRENT TASK: fix add and sub in src/lib.rs"),
        "{first}"
    );
    // After the breaking edit the background verifier's red build reframes
    // the task, and the stated intent is shown back once.
    let third = last_message(&seen[2]);
    assert!(third.contains("Fix the build before continuing"), "{third}");
    assert!(third.contains("Last compilation: Failed."), "{third}");
    assert_eq!(
        third.matches("did: swap the operators").count(),
        1,
        "{third}"
    );
    // Sections in order: task, verify, intents, session state.
    let order: Vec<usize> = ["[Task]", "[Verify]", "[Recent intents]", "[Session state]"]
        .iter()
        .map(|h| {
            third
                .find(h)
                .unwrap_or_else(|| panic!("{h} missing:\n{third}"))
        })
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{third}");
    // After the fix it is green again and back on the task.
    let fourth = last_message(&seen[3]);
    assert!(fourth.contains("Last compilation: Succeeded!"), "{fourth}");
    assert!(fourth.contains("YOUR CURRENT TASK"), "{fourth}");
    // The tool never saw the intent field.
    let results = seen[3]["messages"].to_string();
    assert!(!results.contains("unknown field"), "{results}");

    // Export the session, redacted.
    let export = nyq(work.path(), home.path())
        .args(["export", "last", "--redact", "-o", "-"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&export.stdout);
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    let lines: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect();
    assert!(lines.len() >= 8, "{text}");
    assert!(text.contains("multi_edit"), "{text}");
    assert!(
        text.contains("<ws>"),
        "the workspace path is anonymized: {text}"
    );
    let ws = work.path().to_string_lossy().to_string();
    assert!(!text.contains(&ws), "{text}");
    // The trailing note is send-only: it never reached the stored session.
    assert!(!text.contains("[Session state]"), "{text}");
    assert!(!text.contains("LATEST COMPILE/TEST STATE"), "{text}");

    // The minimal profile's resolved configuration.
    let dump = nyq(work.path(), home.path())
        .args(["--profile", "minimal", "--dump-config"])
        .output()
        .unwrap();
    assert!(
        dump.status.success(),
        "{}",
        String::from_utf8_lossy(&dump.stderr)
    );
    let cfg: Value = serde_json::from_slice(&dump.stdout).unwrap();
    assert_eq!(cfg["profile"], "minimal");
    assert_eq!(cfg["profile_source"], "built-in");
    let enabled = &cfg["settings"]["enabled_tools"];
    assert_eq!(
        enabled["value"],
        json!(["bash", "view_image", "submit"]),
        "{cfg}"
    );
    assert_eq!(enabled["source"], "profile:minimal");
    assert_eq!(cfg["settings"]["features.mcp"]["value"], false);
    // The user's own keys still show, from their layer.
    assert_eq!(cfg["settings"]["verify.debounce_ms"]["source"], "user");
}
