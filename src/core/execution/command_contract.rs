//! Command result contracts shared by foreground and background execution.
//!
//! Ported from `zig-reference/src/core/execution/command_contract.zig`.

use crate::core::background::process_supervisor::ProcessInstanceToken;
use crate::core::shared::types::StableBackgroundRecordId;
use crate::core::tooling::command_output_content::Stream;

pub type CommandOutputStream = Stream;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BackgroundCommand {
    pub pid: Vec<u8>,
    pub process_token: Option<ProcessInstanceToken>,
    pub background_record_id: Option<StableBackgroundRecordId>,
    pub command: Vec<u8>,
    pub cwd: Vec<u8>,
    pub log_path: Vec<u8>,
    pub url: Option<Vec<u8>>,
    pub expect_url: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ForegroundCommandResult {
    pub command: Vec<u8>,
    pub cwd: Vec<u8>,
    pub exit_code: Option<i64>,
    pub signal: Option<u32>,
    pub timed_out: bool,
    pub termination_indeterminate: bool,
    pub duration_ms: Option<u64>,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub truncated: bool,
    pub output_file: Option<Vec<u8>>,
    pub stdout_file: Option<Vec<u8>>,
    pub stderr_file: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BackgroundCommandResult {
    pub command: Vec<u8>,
    pub cwd: Vec<u8>,
    pub background_id: Option<u64>,
    pub pid: Vec<u8>,
    pub log_path: Vec<u8>,
    pub state: Vec<u8>,
    pub server_url: Option<Vec<u8>>,
}

impl BackgroundCommandResult {
    pub fn default_state() -> Vec<u8> {
        b"running".to_vec()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CommandResult {
    Foreground(ForegroundCommandResult),
    Background(BackgroundCommandResult),
}

impl CommandResult {
    pub fn write_json(&self, writer: &mut Vec<u8>) {
        match self {
            Self::Foreground(result) => write_foreground_json(result, writer),
            Self::Background(result) => write_background_json(result, writer),
        }
    }

    pub fn to_json(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write_json(&mut out);
        out
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunCommandResult {
    pub output: Vec<u8>,
    pub background: Option<BackgroundCommand>,
    pub command_result: Option<CommandResult>,
    pub cancelled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForegroundCommandStatus {
    ExitCode(i64),
    Signal(u32),
    Finished,
    Indeterminate,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ForegroundCommandResultSnapshot {
    pub command: Vec<u8>,
    pub cwd: Vec<u8>,
    pub status: ForegroundCommandStatus,
    pub stdout_display: Vec<u8>,
    pub stderr_display: Vec<u8>,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForegroundStatusProjection {
    pub exit_code: Option<i64>,
    pub signal: Option<u32>,
    pub termination_indeterminate: bool,
}

pub fn format_foreground_command_result(
    snapshot: ForegroundCommandResultSnapshot,
) -> RunCommandResult {
    let stdout_text = trim(&snapshot.stdout_display, b" \r\n\t");
    let stderr_text = trim(&snapshot.stderr_display, b" \r\n\t");

    let mut out: Vec<u8> = Vec::new();
    write_foreground_status_line(&mut out, snapshot.status);
    write_foreground_output_envelopes(&mut out, stdout_text, stderr_text);
    let status = project_foreground_status(snapshot.status);

    RunCommandResult {
        output: out,
        background: None,
        command_result: Some(CommandResult::Foreground(ForegroundCommandResult {
            command: snapshot.command,
            cwd: snapshot.cwd,
            exit_code: status.exit_code,
            signal: status.signal,
            termination_indeterminate: status.termination_indeterminate,
            duration_ms: snapshot.duration_ms,
            stdout_bytes: snapshot.stdout_bytes,
            stderr_bytes: snapshot.stderr_bytes,
            ..ForegroundCommandResult::default()
        })),
        cancelled: false,
    }
}

pub fn write_foreground_status_line(writer: &mut Vec<u8>, status: ForegroundCommandStatus) {
    match status {
        ForegroundCommandStatus::ExitCode(code) => {
            writer.extend_from_slice(format!("exit_code={code}\n").as_bytes())
        }
        ForegroundCommandStatus::Signal(signal) => {
            writer.extend_from_slice(format!("signal={signal}\n").as_bytes())
        }
        ForegroundCommandStatus::Finished => {
            writer.extend_from_slice(b"process finished\n")
        }
        ForegroundCommandStatus::Indeterminate => writer.extend_from_slice(
            b"termination_indeterminate=true\nmessage=the command was started, but fx could not confirm its final process status; do not retry unchanged because side effects may already exist\n",
        ),
    }
}

fn write_foreground_output_envelopes(writer: &mut Vec<u8>, stdout_text: &[u8], stderr_text: &[u8]) {
    if !stdout_text.is_empty() {
        writer.extend_from_slice(b"<stdout>\n");
        writer.extend_from_slice(stdout_text);
        writer.extend_from_slice(b"\n</stdout>\n");
    }
    if !stderr_text.is_empty() {
        writer.extend_from_slice(b"<stderr>\n");
        writer.extend_from_slice(stderr_text);
        writer.extend_from_slice(b"\n</stderr>\n");
    }
    if stdout_text.is_empty() && stderr_text.is_empty() {
        writer.extend_from_slice(b"(no output)\n");
    }
}

pub fn project_foreground_status(status: ForegroundCommandStatus) -> ForegroundStatusProjection {
    match status {
        ForegroundCommandStatus::ExitCode(code) => ForegroundStatusProjection {
            exit_code: Some(code),
            signal: None,
            termination_indeterminate: false,
        },
        ForegroundCommandStatus::Signal(signal) => ForegroundStatusProjection {
            exit_code: None,
            signal: Some(signal),
            termination_indeterminate: false,
        },
        ForegroundCommandStatus::Finished => ForegroundStatusProjection {
            exit_code: None,
            signal: None,
            termination_indeterminate: false,
        },
        ForegroundCommandStatus::Indeterminate => ForegroundStatusProjection {
            exit_code: None,
            signal: None,
            termination_indeterminate: true,
        },
    }
}

fn write_foreground_json(result: &ForegroundCommandResult, writer: &mut Vec<u8>) {
    writer.extend_from_slice(b"{\"kind\":\"foreground\"");
    write_string_field(writer, "command", Some(&result.command));
    write_string_field(writer, "cwd", Some(&result.cwd));
    write_optional_int_field(writer, "exit_code", result.exit_code);
    write_optional_int_field(writer, "signal", result.signal);
    write_bool_field(writer, "timed_out", result.timed_out);
    if result.termination_indeterminate {
        write_bool_field(writer, "termination_indeterminate", true);
    }
    write_optional_int_field(writer, "duration_ms", result.duration_ms);
    write_int_field(writer, "stdout_bytes", result.stdout_bytes);
    write_int_field(writer, "stderr_bytes", result.stderr_bytes);
    write_bool_field(writer, "truncated", result.truncated);
    write_string_field(writer, "output_file", result.output_file.as_deref());
    write_string_field(writer, "stdout_file", result.stdout_file.as_deref());
    write_string_field(writer, "stderr_file", result.stderr_file.as_deref());
    writer.push(b'}');
}

fn write_background_json(result: &BackgroundCommandResult, writer: &mut Vec<u8>) {
    writer.extend_from_slice(b"{\"kind\":\"background\"");
    write_string_field(writer, "command", Some(&result.command));
    write_string_field(writer, "cwd", Some(&result.cwd));
    write_optional_int_field(writer, "background_id", result.background_id);
    write_string_field(writer, "pid", Some(&result.pid));
    write_string_field(writer, "log_path", Some(&result.log_path));
    write_string_field(writer, "state", Some(&result.state));
    write_string_field(writer, "server_url", result.server_url.as_deref());
    writer.push(b'}');
}

fn write_json_string(writer: &mut Vec<u8>, bytes: &[u8]) {
    writer.push(b'"');
    for &byte in bytes {
        match byte {
            b'"' => writer.extend_from_slice(b"\\\""),
            b'\\' => writer.extend_from_slice(b"\\\\"),
            b'\n' => writer.extend_from_slice(b"\\n"),
            b'\r' => writer.extend_from_slice(b"\\r"),
            b'\t' => writer.extend_from_slice(b"\\t"),
            0x08 => writer.extend_from_slice(b"\\b"),
            0x0c => writer.extend_from_slice(b"\\f"),
            0x00..=0x1f => writer.extend_from_slice(format!("\\u{byte:04x}").as_bytes()),
            _ => writer.push(byte),
        }
    }
    writer.push(b'"');
}

fn write_string_field(writer: &mut Vec<u8>, name: &str, value: Option<&[u8]>) {
    writer.extend_from_slice(format!(",\"{name}\":").as_bytes());
    match value {
        Some(text) => write_json_string(writer, text),
        None => writer.extend_from_slice(b"null"),
    }
}

fn write_bool_field(writer: &mut Vec<u8>, name: &str, value: bool) {
    writer.extend_from_slice(format!(",\"{name}\":").as_bytes());
    writer.extend_from_slice(if value { b"true" } else { b"false" });
}

fn write_int_field<T: std::fmt::Display>(writer: &mut Vec<u8>, name: &str, value: T) {
    writer.extend_from_slice(format!(",\"{name}\":{value}").as_bytes());
}

fn write_optional_int_field<T: std::fmt::Display>(
    writer: &mut Vec<u8>,
    name: &str,
    value: Option<T>,
) {
    writer.extend_from_slice(format!(",\"{name}\":").as_bytes());
    match value {
        Some(number) => writer.extend_from_slice(format!("{number}").as_bytes()),
        None => writer.extend_from_slice(b"null"),
    }
}

fn trim<'a>(value: &'a [u8], mask: &[u8]) -> &'a [u8] {
    let mut start = 0;
    let mut end = value.len();
    while start < end && mask.contains(&value[start]) {
        start += 1;
    }
    while end > start && mask.contains(&value[end - 1]) {
        end -= 1;
    }
    &value[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_result_preserves_envelopes_metadata_and_json() {
        let result = format_foreground_command_result(ForegroundCommandResultSnapshot {
            command: b"printf hello".to_vec(),
            cwd: b"/tmp".to_vec(),
            status: ForegroundCommandStatus::ExitCode(7),
            stdout_display: b" hello\n".to_vec(),
            stderr_display: b" warn\n".to_vec(),
            stdout_bytes: 7,
            stderr_bytes: 6,
            duration_ms: Some(12),
        });

        assert_eq!(
            result.output,
            b"exit_code=7\n<stdout>\nhello\n</stdout>\n<stderr>\nwarn\n</stderr>\n"
        );
        let CommandResult::Foreground(foreground) = result.command_result.unwrap() else {
            panic!("expected foreground");
        };
        assert_eq!(foreground.command, b"printf hello");
        assert_eq!(foreground.cwd, b"/tmp");
        assert_eq!(foreground.exit_code, Some(7));
        assert_eq!(foreground.signal, None);
        assert_eq!(foreground.duration_ms, Some(12));
        assert_eq!(foreground.stdout_bytes, 7);
        assert_eq!(foreground.stderr_bytes, 6);

        let json = CommandResult::Foreground(foreground).to_json();
        assert_eq!(
            json,
            b"{\"kind\":\"foreground\",\"command\":\"printf hello\",\"cwd\":\"/tmp\",\"exit_code\":7,\"signal\":null,\"timed_out\":false,\"duration_ms\":12,\"stdout_bytes\":7,\"stderr_bytes\":6,\"truncated\":false,\"output_file\":null,\"stdout_file\":null,\"stderr_file\":null}"
        );
    }

    #[test]
    fn foreground_result_preserves_empty_finished_output() {
        let result = format_foreground_command_result(ForegroundCommandResultSnapshot {
            command: b"cmd".to_vec(),
            cwd: b"/tmp".to_vec(),
            status: ForegroundCommandStatus::Finished,
            stdout_display: Vec::new(),
            stderr_display: Vec::new(),
            stdout_bytes: 0,
            stderr_bytes: 0,
            duration_ms: None,
        });
        assert_eq!(result.output, b"process finished\n(no output)\n");
    }

    #[test]
    fn foreground_result_represents_indeterminate_termination_without_implying_no_execution() {
        let result = format_foreground_command_result(ForegroundCommandResultSnapshot {
            command: b"printf effect > marker".to_vec(),
            cwd: b"/tmp".to_vec(),
            status: ForegroundCommandStatus::Indeterminate,
            stdout_display: b"observed output".to_vec(),
            stderr_display: Vec::new(),
            stdout_bytes: 15,
            stderr_bytes: 0,
            duration_ms: None,
        });

        assert!(find(&result.output, b"termination_indeterminate=true").is_some());
        assert!(find(&result.output, b"do not retry unchanged").is_some());
        let CommandResult::Foreground(foreground) = result.command_result.unwrap() else {
            panic!("expected foreground");
        };
        assert!(foreground.termination_indeterminate);
        assert_eq!(foreground.exit_code, None);
        assert_eq!(foreground.signal, None);
        let json = CommandResult::Foreground(foreground).to_json();
        assert!(find(&json, b"\"termination_indeterminate\":true").is_some());
    }

    #[test]
    fn background_json_includes_running_state_and_server_url() {
        let result = CommandResult::Background(BackgroundCommandResult {
            command: b"npm run dev".to_vec(),
            cwd: b"/tmp".to_vec(),
            background_id: Some(3),
            pid: b"4242".to_vec(),
            log_path: b"/tmp/log".to_vec(),
            state: BackgroundCommandResult::default_state(),
            server_url: Some(b"http://127.0.0.1:3000".to_vec()),
        });
        let json = result.to_json();
        assert_eq!(
            json,
            b"{\"kind\":\"background\",\"command\":\"npm run dev\",\"cwd\":\"/tmp\",\"background_id\":3,\"pid\":\"4242\",\"log_path\":\"/tmp/log\",\"state\":\"running\",\"server_url\":\"http://127.0.0.1:3000\"}"
        );
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || haystack.len() < needle.len() {
            return None;
        }
        haystack.windows(needle.len()).position(|w| w == needle)
    }
}
