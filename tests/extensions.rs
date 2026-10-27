//! Extensions end to end: install the Python example with `nyq ext install`,
//! then run print mode against a fake model that calls its tool and a shell
//! command the example's guard blocks, with a second extension rewriting the
//! prompt and a tool result.
#![cfg(unix)]

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

fn delta(d: Value) -> Value {
    json!({"choices": [{"index": 0, "delta": d}]})
}

fn tool_call(id: &str, name: &str, args: Value) -> Value {
    delta(
        json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
        "function": {"name": name, "arguments": args.to_string()}}]}),
    )
}

fn nyq(home: &Path, work: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nyq"));
    cmd.current_dir(work)
        .env("NYQ_HOME", home)
        .env("HOME", home)
        .env("NYQ_MODEL", "fake-model")
        .env_remove("NYQ_API_KEY")
        .stdin(std::process::Stdio::null());
    cmd
}

const REWRITER: &str = r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    m, p = msg.get("method"), msg.get("params") or {}
    if m == "initialize":
        r = {"events": ["user_prompt", "tool_result"], "status": "rw"}
    elif m == "event/user_prompt":
        r = {"prompt": p["prompt"].replace("PLEASE", "please"), "context": "EXT CONTEXT: branch main"}
    elif m == "event/tool_result":
        r = {"content": p["content"] + " [checked]"} if p["tool"].startswith("ext__") else {}
    else:
        r = {}
    if "id" in msg:
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": r}), flush=True)
"#;

#[test]
fn installed_extensions_add_tools_guard_calls_and_rewrite_prompts() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipped: python3 is not available");
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/extensions/word-tools");
    let out = nyq(home.path(), work.path())
        .args(["ext", "install"])
        .arg(&example)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rw = home.path().join("extensions/rewriter");
    std::fs::create_dir_all(&rw).unwrap();
    std::fs::write(rw.join("ext.py"), REWRITER).unwrap();
    std::fs::write(
        rw.join("extension.json"),
        r#"{"name": "rewriter", "command": ["python3", "ext.py"]}"#,
    )
    .unwrap();
    let list = nyq(home.path(), work.path())
        .args(["extensions", "list"])
        .output()
        .unwrap();
    let list = String::from_utf8_lossy(&list.stdout);
    assert!(list.contains("word-tools 0.1.0 · user · enabled"), "{list}");
    assert!(list.contains("rewriter"), "{list}");

    let (url, seen) = fake_server(vec![
        vec![tool_call(
            "c1",
            "ext__word-tools__word_count",
            json!({"text": "one two three"}),
        )],
        vec![tool_call(
            "c2",
            "bash",
            json!({"command": "rm -rf ./nothing-here"}),
        )],
        vec![delta(json!({"content": "Done."}))],
    ]);
    let out = nyq(home.path(), work.path())
        .args([
            "--print",
            "--yolo",
            "--sandbox",
            "off",
            "--provider",
            "openai",
            "count PLEASE",
        ])
        .env("NYQ_BASE_URL", &url)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("Done."));

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "{stderr}");
    let tools = seen[0]["tools"].to_string();
    assert!(tools.contains("ext__word-tools__word_count"), "{tools}");
    let first = seen[0]["messages"].to_string();
    assert!(
        first.contains("count please") && !first.contains("PLEASE"),
        "{first}"
    );
    assert!(first.contains("EXT CONTEXT: branch main"), "{first}");
    let second = seen[1]["messages"].to_string();
    assert!(
        second.contains("1 lines, 3 words, 13 characters [checked]"),
        "{second}"
    );
    let third = seen[2]["messages"].to_string();
    assert!(
        third.contains("rm -rf is blocked by the word-tools extension"),
        "{third}"
    );

    let log = std::fs::read_to_string(home.path().join("logs/ext-word-tools.log")).unwrap();
    assert!(log.contains("started for"), "{log}");

    let out = nyq(home.path(), work.path())
        .args(["ext", "remove", "word-tools"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!home.path().join("extensions/word-tools").exists());
}
