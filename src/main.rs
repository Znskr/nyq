//! Thin binary wrapper around the `nyq` library harness.

use std::process::ExitCode;

fn main() -> ExitCode {
    let code = nyq::app::run(std::env::args().skip(1));
    ExitCode::from(code.clamp(0, 255) as u8)
}
