//! Guided compatible-provider setup through the real CLI, using local endpoints.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Command, Output, Stdio};
use std::thread;

use serde_json::{json, Value};

fn endpoint(status: u16) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            request.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        let body = if status == 200 {
            json!({"data":[{"id":"test-model"}]}).to_string()
        } else {
            // A gateway echoing a credential must not expose it in diagnostics.
            json!({"error":"rejected secret-test-key"}).to_string()
        };
        write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        request
    });
    (url, task)
}

fn cli(home: &std::path::Path, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nyq"))
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("NYQ_HOME", home)
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn success(out: &Output) {
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("secret-test-key"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("secret-test-key"));
}

#[test]
fn guided_setup_discovers_both_protocols_and_remembers_default() {
    let home = tempfile::tempdir().unwrap();
    for (protocol, id, authorization) in [
        (1, "my-openai", "authorization: bearer secret-test-key"),
        (2, "my-anthropic", "x-api-key: secret-test-key"),
    ] {
        let (url, task) = endpoint(200);
        let out = cli(
            home.path(),
            &["login", "custom"],
            &format!("{protocol}\n{id}\n{url}\n1\nsecret-test-key\n1\n"),
        );
        success(&out);
        let request = task.join().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /v1/models "), "{request}");
        assert!(request.contains(authorization), "{request}");
        success(&cli(home.path(), &["provider", id], ""));
        let last: Value =
            serde_json::from_slice(&std::fs::read(home.path().join("last-model.json")).unwrap())
                .unwrap();
        assert_eq!(last["provider"], id);
        assert_eq!(last["model"], "test-model");
    }
    let definitions: Value =
        serde_json::from_slice(&std::fs::read(home.path().join("providers.json")).unwrap())
            .unwrap();
    assert_eq!(definitions.as_array().unwrap().len(), 2);
    success(&cli(home.path(), &["logout", "my-openai"], ""));
    let definitions: Value =
        serde_json::from_slice(&std::fs::read(home.path().join("providers.json")).unwrap())
            .unwrap();
    assert!(definitions[0]["api_key"].is_null());
    assert_eq!(definitions[1]["api_key"], "secret-test-key");
}

#[test]
fn guided_setup_rejects_bad_keys_without_saving_or_echoing_them() {
    let home = tempfile::tempdir().unwrap();
    let (url, task) = endpoint(401);
    let out = cli(
        home.path(),
        &["login", "custom"],
        &format!("1\nrejected\n{url}\n1\nsecret-test-key\n"),
    );
    assert!(!out.status.success());
    assert!(!home.path().join("providers.json").exists());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("secret-test-key"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("secret-test-key"));
    task.join().unwrap();
}

#[test]
fn guided_setup_uses_manual_model_when_discovery_is_unsupported() {
    let home = tempfile::tempdir().unwrap();
    let (url, task) = endpoint(404);
    let out = cli(
        home.path(),
        &["login", "custom"],
        &format!("2\nlocal-gateway\n{url}\n2\nmanual-model\n"),
    );
    success(&out);
    task.join().unwrap();
    let definitions: Value =
        serde_json::from_slice(&std::fs::read(home.path().join("providers.json")).unwrap())
            .unwrap();
    assert_eq!(definitions[0]["model"], "manual-model");
    assert!(definitions[0]["api_key"].is_null());
}
