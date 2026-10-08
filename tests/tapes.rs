//! Exercise the repair and playback CLI without a provider or network.
use std::fs;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn run(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nyq"))
        .args(args)
        .env("HOME", home)
        .env("NYQ_HOME", home)
        .output()
        .unwrap()
}

#[test]
fn replay_emits_output_at_scaled_times_and_rejects_bad_speed() {
    let home = tempfile::tempdir().unwrap();
    let tape = home.path().join("test.cast");
    fs::write(&tape, "{\"version\":2,\"width\":80,\"height\":24}\n[0,\"o\",\"first\\r\\n\"]\n[0.12,\"o\",\"last\"]\n").unwrap();
    let path = tape.to_str().unwrap();
    let start = Instant::now();
    let output = run(home.path(), &["replay", path, "--speed", "3"]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"first\r\nlast");
    assert!(start.elapsed() >= Duration::from_millis(40));
    for speed in ["0", "NaN", "inf"] {
        let output = run(home.path(), &["replay", path, "--speed", speed]);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn session_cli_migrates_and_recovers_without_changing_corrupt_source() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("sessions");
    fs::create_dir(&dir).unwrap();
    let path = dir.join("legacy.jsonl");
    fs::write(&path, "{\"type\":\"user\",\"text\":\"kept\"}\n").unwrap();
    let output = run(home.path(), &["session", "migrate", "legacy"]);
    assert!(output.status.success(), "{:?}", output.stderr);
    let migrated = fs::read(&path).unwrap();
    let output = run(home.path(), &["session", "migrate", "legacy"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("already current"));
    assert_eq!(fs::read(&path).unwrap(), migrated);
    let mut corrupt = migrated;
    corrupt.extend_from_slice(b"{bad}\n{\"type\":\"assistant\",\"text\":\"also kept\"}\n{torn");
    fs::write(&path, &corrupt).unwrap();
    let output = run(home.path(), &["session", "recover", "legacy"]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(String::from_utf8_lossy(&output.stdout).contains("2 items, 2 broken lines skipped"));
    assert_eq!(fs::read(&path).unwrap(), corrupt);
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
}
