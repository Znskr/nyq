//! WASM panic handler that writes straight to stderr and traps, avoiding the
//! default handler's atomics-dependent futex wait.

/// Custom panic entry point. Writes `msg` to stderr and traps instead of
/// unwinding, mirroring `std.debug.FullPanic(panicToStderr)` in the Zig source.
#[allow(non_upper_case_globals)]
pub const panic: fn(&str, Option<usize>) -> ! = panic_to_stderr;

pub fn panic_to_stderr(msg: &str, _first_trace_addr: Option<usize>) -> ! {
    eprintln!("panic: {msg}");
    trap()
}

#[cfg(target_arch = "wasm32")]
fn trap() -> ! {
    core::arch::wasm32::unreachable()
}

#[cfg(not(target_arch = "wasm32"))]
fn trap() -> ! {
    std::process::abort()
}
