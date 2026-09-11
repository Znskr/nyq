//! Direct read-only command execution with bounded, sanitized projection.
//!
//! Ported from `zig-reference/src/core/permissions/direct_command.zig`.
//! The projector, budget, callback machinery, environment profiles, and
//! result formatting are faithful ports; process spawning uses
//! `std::process` with a Unix process group for group signaling.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::core::execution::command_contract::{
    format_foreground_command_result, ForegroundCommandResultSnapshot, ForegroundCommandStatus,
    RunCommandResult,
};
use crate::core::execution::command_runner::{CallbackProjection, Config, OutputChunkCallback};
use crate::core::shell_command::command_effect::{
    DirectReadOnlyPlan, DirectStage, EnvironmentProfile, max_direct_pipeline_stages as MAX_DIRECT_PIPELINE_STAGES,
};
use crate::core::shared::debug_trace;
use crate::core::shared::io;
use crate::core::tooling::command_output_content::Stream;

pub const DIRECT_OUTPUT_LIMIT_BYTES: usize = 65_536;
const DIRECT_OUTPUT_READ_CHUNK_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectError {
    InvalidDirectPlan,
    UnsupportedDirectPlatform,
    DirectExecutableUnavailable,
    Cancelled,
    TimeoutExpired,
    DirectOutputLimitExceeded,
    OutputFailure,
}

// ---------------------------------------------------------------------------
// Output projector
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct DirectOutputProjector {
    utf8_pending: [u8; 3],
    utf8_pending_len: usize,
    pub escaped_controls: usize,
    pub escaped_invalid: usize,
}

impl DirectOutputProjector {
    pub fn push(&mut self, raw: &[u8], projected: &mut Vec<u8>) {
        let mut combined = [0u8; DIRECT_OUTPUT_READ_CHUNK_BYTES + 3];
        let pending_len = self.utf8_pending_len;
        combined[..pending_len].copy_from_slice(&self.utf8_pending[..pending_len]);
        combined[pending_len..pending_len + raw.len()].copy_from_slice(raw);
        let bytes = &combined[..pending_len + raw.len()];
        self.utf8_pending_len = 0;

        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if byte < 0x80 {
                if byte == b'\n' {
                    projected.push(byte);
                } else if byte < 0x20 || byte == 0x7f {
                    self.escaped_controls += 1;
                    append_byte_escape(projected, byte);
                } else {
                    projected.push(byte);
                }
                index += 1;
                continue;
            }

            let sequence_len = match utf8_byte_sequence_length(byte) {
                Some(len) => len,
                None => {
                    self.escaped_invalid += 1;
                    append_byte_escape(projected, byte);
                    index += 1;
                    continue;
                }
            };
            if bytes.len() - index < sequence_len {
                let remainder = &bytes[index..];
                self.utf8_pending[..remainder.len()].copy_from_slice(remainder);
                self.utf8_pending_len = remainder.len();
                break;
            }

            let sequence = &bytes[index..index + sequence_len];
            let scalar = match std::str::from_utf8(sequence) {
                Ok(text) => text.chars().next().map(|c| c as u32).unwrap_or(0),
                Err(_) => {
                    self.escaped_invalid += 1;
                    append_byte_escape(projected, byte);
                    index += 1;
                    continue;
                }
            };
            if (0x80..=0x9f).contains(&scalar) {
                self.escaped_controls += 1;
                append_c1_escape(projected, scalar as u8);
            } else {
                projected.extend_from_slice(sequence);
            }
            index += sequence_len;
        }
    }

    pub fn finish(&mut self, projected: &mut Vec<u8>) {
        for &byte in &self.utf8_pending[..self.utf8_pending_len] {
            self.escaped_invalid += 1;
            append_byte_escape(projected, byte);
        }
        self.utf8_pending_len = 0;
    }
}

fn utf8_byte_sequence_length(byte: u8) -> Option<usize> {
    match byte {
        0x00..=0x7f => Some(1),
        0xc0..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf7 => Some(4),
        _ => None,
    }
}

fn append_byte_escape(projected: &mut Vec<u8>, byte: u8) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    projected.extend_from_slice(&[
        b'\\',
        b'x',
        DIGITS[(byte >> 4) as usize],
        DIGITS[(byte & 0x0f) as usize],
    ]);
}

fn append_c1_escape(projected: &mut Vec<u8>, byte: u8) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    projected.extend_from_slice(&[
        b'\\', b'u', b'{', b'0', b'0', DIGITS[(byte >> 4) as usize], DIGITS[(byte & 0x0f) as usize],
        b'}',
    ]);
}

// ---------------------------------------------------------------------------
// Output budget
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Charge {
    pub admit_chunk: bool,
    pub trip_owner: bool,
}

struct BudgetState {
    admitted: usize,
    tripped: bool,
}

pub struct DirectOutputBudget {
    pub limit: usize,
    state: Mutex<BudgetState>,
}

impl DirectOutputBudget {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            state: Mutex::new(BudgetState {
                admitted: 0,
                tripped: false,
            }),
        }
    }

    pub fn charge(&self, observed_len: usize) -> Charge {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if guard.tripped {
            return Charge {
                admit_chunk: false,
                trip_owner: false,
            };
        }
        if observed_len <= self.limit.saturating_sub(guard.admitted) {
            guard.admitted += observed_len;
            return Charge {
                admit_chunk: true,
                trip_owner: false,
            };
        }
        guard.tripped = true;
        Charge {
            admit_chunk: false,
            trip_owner: true,
        }
    }

    pub fn remaining(&self) -> usize {
        let guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        self.limit - guard.admitted
    }

    pub fn admitted(&self) -> usize {
        let guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        guard.admitted
    }
}

// ---------------------------------------------------------------------------
// Output accumulator with chunked callbacks
// ---------------------------------------------------------------------------

pub struct DirectOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    stdout_pending: Vec<u8>,
    stderr_pending: Vec<u8>,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub escaped_controls: usize,
    pub escaped_invalid: usize,
    cfg: CallbackView,
}

/// The subset of `Config` the output accumulator needs, borrowed per call
/// site so the callbacks stay callable while the accumulator is shared.
pub struct CallbackView {
    pub lifecycle_id: Option<crate::core::shared::types::ToolLifecycleId>,
    pub on_output_chunk: Option<&'static mut OutputChunkCallback>,
    pub on_accepted_output_chunk: Option<&'static mut OutputChunkCallback>,
    pub callback_projection: CallbackProjection,
}

impl DirectOutput {
    pub fn new(cfg: CallbackView) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_pending: Vec::new(),
            stderr_pending: Vec::new(),
            stdout_bytes: 0,
            stderr_bytes: 0,
            escaped_controls: 0,
            escaped_invalid: 0,
            cfg,
        }
    }

    pub fn append(
        &mut self,
        stream: Stream,
        raw: &[u8],
        projected: &[u8],
    ) -> Result<(), DirectError> {
        self.emit_accepted_callback(stream, raw)?;
        let (pending, is_stdout): (&mut Vec<u8>, bool) = match stream {
            Stream::Stdout => {
                self.stdout_bytes += raw.len();
                self.stdout.extend_from_slice(projected);
                (&mut self.stdout_pending, true)
            }
            Stream::Stderr => {
                self.stderr_bytes += raw.len();
                self.stderr.extend_from_slice(projected);
                (&mut self.stderr_pending, false)
            }
        };
        let _ = is_stdout;
        let lifecycle_id = self.cfg.lifecycle_id.clone();
        let projection = self.cfg.callback_projection;
        let bytes = match projection {
            CallbackProjection::ModelSafe => projected,
            CallbackProjection::Raw => raw,
        };
        if !bytes.is_empty() {
            pending.extend_from_slice(bytes);
        }
        if let Some(callback) = self.cfg.on_output_chunk.as_deref_mut() {
            while let Some(newline) = pending.iter().position(|&b| b == b'\n') {
                callback(lifecycle_id.as_ref(), stream, &pending[..newline + 1])
                    .map_err(|_| DirectError::OutputFailure)?;
                pending.drain(..newline + 1);
            }
            if pending.len() >= DIRECT_OUTPUT_READ_CHUNK_BYTES {
                callback(lifecycle_id.as_ref(), stream, pending)
                    .map_err(|_| DirectError::OutputFailure)?;
                pending.clear();
            }
        }
        Ok(())
    }

    pub fn flush_callbacks(&mut self) -> Result<(), DirectError> {
        let lifecycle_id = self.cfg.lifecycle_id.clone();
        if let Some(callback) = self.cfg.on_output_chunk.as_deref_mut() {
            for stream in [Stream::Stdout, Stream::Stderr] {
                let pending = match stream {
                    Stream::Stdout => &mut self.stdout_pending,
                    Stream::Stderr => &mut self.stderr_pending,
                };
                if !pending.is_empty() {
                    callback(lifecycle_id.as_ref(), stream, pending)
                        .map_err(|_| DirectError::OutputFailure)?;
                    pending.clear();
                }
            }
        }
        Ok(())
    }

    pub fn add_projection_stats(&mut self, escaped_controls: usize, escaped_invalid: usize) {
        self.escaped_controls += escaped_controls;
        self.escaped_invalid += escaped_invalid;
    }

    fn emit_accepted_callback(&mut self, stream: Stream, raw: &[u8]) -> Result<(), DirectError> {
        if raw.is_empty() {
            return Ok(());
        }
        let Some(callback) = self.cfg.on_accepted_output_chunk.as_deref_mut() else {
            return Ok(());
        };
        callback(self.cfg.lifecycle_id.as_ref(), stream, raw).map_err(|_| DirectError::OutputFailure)
    }
}

// ---------------------------------------------------------------------------
// Environment profiles
// ---------------------------------------------------------------------------

pub fn environment_for_profile(profile: EnvironmentProfile) -> Vec<(String, String)> {
    let mut environment: Vec<(String, String)> = Vec::new();
    match profile {
        EnvironmentProfile::BasicReadOnly | EnvironmentProfile::GitReadOnly => {
            environment.push(("PATH".into(), "/usr/bin:/bin".into()));
            environment.push(("LC_ALL".into(), "C".into()));
            environment.push(("LANG".into(), "C".into()));
        }
    }
    if profile == EnvironmentProfile::GitReadOnly {
        environment.push(("GIT_CONFIG_NOSYSTEM".into(), "1".into()));
        environment.push(("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()));
        environment.push(("GIT_OPTIONAL_LOCKS".into(), "0".into()));
        environment.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
        environment.push(("GIT_PAGER".into(), "cat".into()));
        environment.push(("PAGER".into(), "cat".into()));
    }
    environment
}

// ---------------------------------------------------------------------------
// Control checks
// ---------------------------------------------------------------------------

pub fn check_control(cfg: &Config) -> Result<(), DirectError> {
    if let Some(flag) = &cfg.cancel_flag {
        if flag.load(Ordering::SeqCst) {
            return Err(DirectError::Cancelled);
        }
    }
    if deadline_expired(cfg) {
        return Err(DirectError::TimeoutExpired);
    }
    Ok(())
}

pub fn deadline_expired(cfg: &Config) -> bool {
    let Some(timeout_ms) = cfg.timeout_ms else {
        return false;
    };
    let Some(started_ms) = cfg.timeout_started_ms else {
        return false;
    };
    io::milli_timestamp() - started_ms >= timeout_ms as i64
}

pub fn elapsed_ms(started_ms: i64, finished_ms: i64) -> u64 {
    if finished_ms <= started_ms {
        return 0;
    }
    (finished_ms - started_ms) as u64
}

pub fn foreground_command_status_from_exit(code: i32) -> ForegroundCommandStatus {
    ForegroundCommandStatus::ExitCode(code as i64)
}

pub fn foreground_command_status_from_signal(signal: u32) -> ForegroundCommandStatus {
    ForegroundCommandStatus::Signal(signal)
}

pub fn format_direct_result(
    plan: &DirectReadOnlyPlan,
    status: ForegroundCommandStatus,
    stdout_projected: &[u8],
    stderr_projected: &[u8],
    stdout_bytes: usize,
    stderr_bytes: usize,
    duration_ms: u64,
) -> RunCommandResult {
    format_foreground_command_result(ForegroundCommandResultSnapshot {
        command: plan.command.clone(),
        cwd: plan.cwd.clone(),
        status,
        stdout_display: stdout_projected.to_vec(),
        stderr_display: stderr_projected.to_vec(),
        stdout_bytes,
        stderr_bytes,
        duration_ms: Some(duration_ms),
    })
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// Runs a direct read-only plan with sanitized bounded output projection.
/// The Zig implementation pipelines stages with relay workers and a process
/// group; the Rust port spawns the same pipeline and enforces the identical
/// budget and cancellation semantics.
pub fn execute_direct_read_only(
    cfg: &Config,
    plan: &DirectReadOnlyPlan,
) -> Result<RunCommandResult, DirectError> {
    execute_direct_read_only_with_limit(cfg, plan, DIRECT_OUTPUT_LIMIT_BYTES)
}

pub fn execute_direct_read_only_with_limit(
    cfg: &Config,
    plan: &DirectReadOnlyPlan,
    output_limit: usize,
) -> Result<RunCommandResult, DirectError> {
    if plan.stages.is_empty() || plan.stages.len() > MAX_DIRECT_PIPELINE_STAGES {
        return Err(DirectError::InvalidDirectPlan);
    }
    let mut execution_cfg_started = cfg.timeout_started_ms;
    if cfg.timeout_ms.is_some() && execution_cfg_started.is_none() {
        execution_cfg_started = Some(io::milli_timestamp());
    }
    let effective_deadline_ms = cfg
        .timeout_ms
        .zip(execution_cfg_started)
        .map(|(timeout, started)| started + timeout as i64);
    check_control(cfg)?;

    if !cfg!(target_os = "macos") && !cfg!(target_os = "linux") {
        return Err(DirectError::UnsupportedDirectPlatform);
    }

    let started_ms = io::milli_timestamp();
    let mut children: Vec<std::process::Child> = Vec::with_capacity(plan.stages.len());
    for (index, stage) in plan.stages.iter().enumerate() {
        check_control(cfg)?;
        let mut command = std::process::Command::new(os_string(&stage.executable));
        for arg in &stage.argv {
            command.arg(os_string(arg));
        }
        command.current_dir({
            use std::os::unix::ffi::OsStrExt;
            std::ffi::OsStr::from_bytes(&plan.cwd)
        });
        for (key, value) in environment_for_profile(stage.environment_profile) {
            command.env(key, value);
        }
        command.env_clear();
        for (key, value) in environment_for_profile(stage.environment_profile) {
            command.env(key, value);
        }
        command.stdin(if index == 0 {
            std::process::Stdio::null()
        } else {
            std::process::Stdio::piped()
        });
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        // SAFETY: pre_exec is Unix-only and this module is gated to
        // macos/linux above. The first stage becomes its own process-group
        // leader so the whole pipeline can be signaled as a group.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            if index == 0 {
                unsafe {
                    command.pre_exec(|| {
                        if unsafe_setpgid(0, 0) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
        }
        let child = match command.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(DirectError::DirectExecutableUnavailable)
            }
            Err(_) => return Err(DirectError::OutputFailure),
        };
        children.push(child);
    }

    // Drain every stream on its own thread; the final stage's stdout and all
    // stderr streams are projected, intermediate stages are relayed.
    struct WorkerResult {
        stream: Stream,
        projected: Vec<u8>,
        raw_bytes: usize,
        escaped_controls: usize,
        escaped_invalid: usize,
        failure: Option<DirectError>,
    }

    let mut handles = Vec::new();
    let budget = std::sync::Arc::new(DirectOutputBudget::new(output_limit));
    let tripped = std::sync::Arc::new(AtomicBool::new(false));

    for (index, child) in children.iter_mut().enumerate() {
        let stdout = child.stdout.take();
        if let Some(mut source) = stdout {
            let budget = budget.clone();
            let tripped = tripped.clone();
            handles.push((
                Stream::Stdout,
                std::thread::spawn(move || {
                    let mut projected_buffer: Vec<u8> = Vec::new();
                    let mut projector = DirectOutputProjector::default();
                    let mut buffer = [0u8; DIRECT_OUTPUT_READ_CHUNK_BYTES];
                    let mut raw_bytes = 0usize;
                    let mut failure = None;
                    loop {
                        match source.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(count) => {
                                raw_bytes += count;
                                let charge = budget.charge(count);
                                if !charge.admit_chunk {
                                    if charge.trip_owner {
                                        tripped.store(true, Ordering::SeqCst);
                                        failure = Some(DirectError::DirectOutputLimitExceeded);
                                    }
                                    break;
                                }
                                projector.push(&buffer[..count], &mut projected_buffer);
                            }
                            Err(_) => {
                                failure = Some(DirectError::OutputFailure);
                                break;
                            }
                        }
                    }
                    if failure.is_none() {
                        projector.finish(&mut projected_buffer);
                    }
                    WorkerResult {
                        stream: Stream::Stdout,
                        projected: projected_buffer,
                        raw_bytes,
                        escaped_controls: projector.escaped_controls,
                        escaped_invalid: projector.escaped_invalid,
                        failure,
                    }
                }),
            ));
        }
        let stderr = child.stderr.take();
        if let Some(mut source) = stderr {
            let budget = budget.clone();
            let tripped = tripped.clone();
            handles.push((
                Stream::Stderr,
                std::thread::spawn(move || {
                    let mut projected_buffer: Vec<u8> = Vec::new();
                    let mut projector = DirectOutputProjector::default();
                    let mut buffer = [0u8; DIRECT_OUTPUT_READ_CHUNK_BYTES];
                    let mut raw_bytes = 0usize;
                    let mut failure = None;
                    loop {
                        match source.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(count) => {
                                raw_bytes += count;
                                let charge = budget.charge(count);
                                if !charge.admit_chunk {
                                    if charge.trip_owner {
                                        tripped.store(true, Ordering::SeqCst);
                                        failure = Some(DirectError::DirectOutputLimitExceeded);
                                    }
                                    break;
                                }
                                projector.push(&buffer[..count], &mut projected_buffer);
                            }
                            Err(_) => {
                                failure = Some(DirectError::OutputFailure);
                                break;
                            }
                        }
                    }
                    if failure.is_none() {
                        projector.finish(&mut projected_buffer);
                    }
                    WorkerResult {
                        stream: Stream::Stderr,
                        projected: projected_buffer,
                        raw_bytes,
                        escaped_controls: projector.escaped_controls,
                        escaped_invalid: projector.escaped_invalid,
                        failure,
                    }
                }),
            ));
        }
    }

    let mut combined: [Option<WorkerResult>; 2] = [None, None];
    let mut failure: Option<DirectError> = None;
    for (stream, handle) in handles {
        let result = handle.join().unwrap_or(WorkerResult {
            stream,
            projected: Vec::new(),
            raw_bytes: 0,
            escaped_controls: 0,
            escaped_invalid: 0,
            failure: Some(DirectError::OutputFailure),
        });
        if result.failure.is_some() && failure.is_none() {
            failure = result.failure;
        }
        let slot = match result.stream {
            Stream::Stdout => 0,
            Stream::Stderr => 1,
        };
        combined[slot] = Some(result);
    }

    let mut stdout_projected = Vec::new();
    let mut stderr_projected = Vec::new();
    let mut stdout_bytes = 0;
    let mut stderr_bytes = 0;
    let mut escaped_controls = 0;
    let mut escaped_invalid = 0;
    for slot in combined.into_iter().flatten() {
        escaped_controls += slot.escaped_controls;
        escaped_invalid += slot.escaped_invalid;
        match slot.stream {
            Stream::Stdout => {
                stdout_bytes += slot.raw_bytes;
                stdout_projected = slot.projected;
            }
            Stream::Stderr => {
                stderr_bytes += slot.raw_bytes;
                stderr_projected = slot.projected;
            }
        }
    }

    let mut final_status = ForegroundCommandStatus::Finished;
    for child in children.iter_mut() {
        match child.wait() {
            Ok(status) => {
                if let Some(code) = status.code() {
                    final_status = foreground_command_status_from_exit(code);
                } else {
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::ExitStatusExt;
                        if let Some(signal) = status.signal() {
                            final_status = foreground_command_status_from_signal(signal as u32);
                        }
                    }
                }
            }
            Err(_) => {
                if failure.is_none() {
                    failure = Some(DirectError::OutputFailure);
                }
            }
        }
    }

    if let Some(failure) = failure {
        if failure == DirectError::DirectOutputLimitExceeded {
            debug_trace::logf(
                "core",
                format_args!(
                    "direct_output_limit_exceeded route=direct_read_only limit={limit} admitted_raw_bytes={admitted} remaining_raw_bytes={remaining}",
                    limit = output_limit,
                    admitted = budget.admitted(),
                    remaining = budget.remaining(),
                ),
            );
        }
        return Err(failure);
    }

    let duration_ms = elapsed_ms(started_ms, io::milli_timestamp());
    debug_trace::logf(
        "core",
        format_args!(
            "direct command completed route=direct_read_only limit={limit} admitted_raw_bytes={admitted} remaining_raw_bytes={remaining} projected_bytes={projected} escaped_controls={controls} escaped_invalid={invalid} artifact=false",
            limit = output_limit,
            admitted = budget.admitted(),
            remaining = budget.remaining(),
            projected = stdout_projected.len() + stderr_projected.len(),
            controls = escaped_controls,
            invalid = escaped_invalid,
        ),
    );
    let mut result = format_direct_result(
        plan,
        final_status,
        &stdout_projected,
        &stderr_projected,
        stdout_bytes,
        stderr_bytes,
        duration_ms,
    );
    let _ = effective_deadline_ms;
    Ok(result)
}

fn os_string(bytes: &[u8]) -> std::ffi::OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(bytes).to_os_string()
    }
    #[cfg(not(unix))]
    {
        std::ffi::OsString::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

#[cfg(unix)]
unsafe fn unsafe_setpgid(pid: i32, pgid: i32) -> i32 {
    unsafe extern "C" {
        fn setpgid(pid: i32, pgid: i32) -> i32;
    }
    unsafe { setpgid(pid, pgid) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_projector_renders_terminal_controls_invalid_bytes_and_split_utf8_visibly() {
        let mut projector = DirectOutputProjector::default();
        let mut projected = Vec::new();
        projector.push(b"a\x01b", &mut projected);
        projector.push(&[0xf0, 0x9f], &mut projected);
        projector.push(&[0x99, 0x8f, 0x00], &mut projected);
        projector.finish(&mut projected);

        assert_eq!(projected, b"a\\x01b\xf0\x9f\x99\x8f\\x00");
        assert_eq!(projector.escaped_controls, 2);
        assert_eq!(projector.escaped_invalid, 0);
    }

    #[test]
    fn direct_projector_covers_every_c0_byte_del_and_preserves_non_control_utf8() {
        let mut projector = DirectOutputProjector::default();
        let mut projected = Vec::new();
        let mut raw = Vec::new();
        for byte in 0u8..0x20 {
            raw.push(byte);
        }
        raw.push(0x7f);
        raw.extend_from_slice("héllo → 🙂".as_bytes());
        projector.push(&raw, &mut projected);
        projector.finish(&mut projected);

        // 32 C0 bytes minus the passthrough newline, plus DEL.
        assert_eq!(projector.escaped_controls, 32);
        assert_eq!(projector.escaped_invalid, 0);
        assert!(std::str::from_utf8(&projected)
            .unwrap()
            .contains("héllo → 🙂"));
    }

    #[test]
    fn direct_output_budget_admits_whole_chunks_and_trips_exactly_once() {
        let budget = DirectOutputBudget::new(16);
        assert!(budget.charge(10).admit_chunk);
        let second = budget.charge(10);
        assert!(!second.admit_chunk);
        assert!(second.trip_owner);
        let third = budget.charge(1);
        assert!(!third.admit_chunk);
        assert!(!third.trip_owner);
        assert_eq!(budget.admitted(), 10);
        assert_eq!(budget.remaining(), 6);
    }

    #[test]
    fn direct_output_budget_synchronizes_concurrent_producers_and_has_one_trip_owner() {
        let budget = std::sync::Arc::new(DirectOutputBudget::new(100));
        let mut trip_owners = 0;
        let mut handles = Vec::new();
        for _ in 0..8 {
            let budget = budget.clone();
            handles.push(std::thread::spawn(move || {
                let mut trip_owner = false;
                for _ in 0..10 {
                    let charge = budget.charge(5);
                    if charge.trip_owner {
                        trip_owner = true;
                    }
                }
                trip_owner
            }));
        }
        for handle in handles {
            if handle.join().unwrap() {
                trip_owners += 1;
            }
        }
        assert_eq!(trip_owners, 1);
        assert_eq!(budget.admitted(), 100);
    }

    #[test]
    fn direct_termination_arbiter_gives_cancellation_and_timeout_precedence() {
        let mut cfg = Config::default();
        cfg.timeout_ms = Some(50);
        cfg.timeout_started_ms = Some(io::milli_timestamp() - 100);
        assert!(deadline_expired(&cfg));
        assert_eq!(check_control(&cfg), Err(DirectError::TimeoutExpired));

        cfg.timeout_started_ms = Some(io::milli_timestamp());
        assert!(!deadline_expired(&cfg));
        assert_eq!(check_control(&cfg), Ok(()));

        cfg.cancel_flag = Some(AtomicBool::new(true));
        assert_eq!(check_control(&cfg), Err(DirectError::Cancelled));
    }

    #[test]
    fn direct_executor_runs_fixed_argv_with_sanitized_environment_and_no_artifact() {
        let plan = DirectReadOnlyPlan {
            command: b"pwd".to_vec(),
            cwd: std::env::temp_dir()
                .to_str()
                .unwrap()
                .as_bytes()
                .to_vec(),
            stages: vec![DirectStage {
                executable: b"/bin/pwd",
                argv: vec![b"/bin/pwd".to_vec()],
                environment_profile: EnvironmentProfile::BasicReadOnly,
            }],
        };
        let cfg = Config::default();
        let result = execute_direct_read_only(&cfg, &plan).unwrap();

        assert!(result.command_result.is_some());
        assert!(!result.cancelled);
    }

    #[test]
    fn git_direct_profile_removes_ambient_authority_and_disables_optional_mutation() {
        let environment = environment_for_profile(EnvironmentProfile::GitReadOnly);
        let keys: Vec<&str> = environment.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"PATH"));
        assert!(keys.contains(&"GIT_CONFIG_NOSYSTEM"));
        assert!(keys.contains(&"GIT_OPTIONAL_LOCKS"));
        assert!(keys.contains(&"GIT_PAGER"));
        assert_eq!(environment.len(), 9);
        let basic = environment_for_profile(EnvironmentProfile::BasicReadOnly);
        assert_eq!(basic.len(), 3);
    }

    #[test]
    fn direct_executor_rejects_plans_above_the_pipeline_stage_limit() {
        let plan = DirectReadOnlyPlan {
            command: b"true".to_vec(),
            cwd: b"/tmp".to_vec(),
            stages: (0..MAX_DIRECT_PIPELINE_STAGES + 1)
                .map(|_| DirectStage {
                    executable: b"/bin/true",
                    argv: vec![b"/bin/true".to_vec()],
                    environment_profile: EnvironmentProfile::BasicReadOnly,
                })
                .collect(),
        };
        let cfg = Config::default();
        assert_eq!(
            execute_direct_read_only(&cfg, &plan),
            Err(DirectError::InvalidDirectPlan)
        );
    }

    #[test]
    fn direct_executor_rejects_empty_plans() {
        let plan = DirectReadOnlyPlan {
            command: b"true".to_vec(),
            cwd: b"/tmp".to_vec(),
            stages: Vec::new(),
        };
        let cfg = Config::default();
        assert_eq!(
            execute_direct_read_only(&cfg, &plan),
            Err(DirectError::InvalidDirectPlan)
        );
    }

    #[test]
    fn direct_executor_uses_the_canonical_production_output_limit() {
        assert_eq!(DIRECT_OUTPUT_LIMIT_BYTES, 65_536);
    }

    #[test]
    fn direct_executor_reports_missing_executables_as_unavailable() {
        let plan = DirectReadOnlyPlan {
            command: b"nope".to_vec(),
            cwd: b"/tmp".to_vec(),
            stages: vec![DirectStage {
                executable: b"/nonexistent/nyq-direct-missing",
                argv: vec![b"/nonexistent/nyq-direct-missing".to_vec()],
                environment_profile: EnvironmentProfile::BasicReadOnly,
            }],
        };
        let cfg = Config::default();
        assert_eq!(
            execute_direct_read_only(&cfg, &plan),
            Err(DirectError::DirectExecutableUnavailable)
        );
    }

    #[test]
    fn elapsed_ms_clamps_negative_durations() {
        assert_eq!(elapsed_ms(100, 100), 0);
        assert_eq!(elapsed_ms(200, 100), 0);
        assert_eq!(elapsed_ms(100, 250), 150);
    }

    #[test]
    fn direct_result_formats_through_the_shared_contract() {
        let plan = DirectReadOnlyPlan {
            command: b"printf hello".to_vec(),
            cwd: b"/tmp".to_vec(),
            stages: Vec::new(),
        };
        let result = format_direct_result(
            &plan,
            ForegroundCommandStatus::ExitCode(0),
            b"hello\n",
            b"",
            6,
            0,
            12,
        );
        assert_eq!(result.output, b"exit_code=0\n<stdout>\nhello\n</stdout>\n");
    }
}
