//! nyq — tiny native coding-agent harness.
//!
//! Everything lives in the library so the harness can be embedded in other
//! applications; `main.rs` is a thin binary wrapper.

pub mod app;
pub mod cli;
pub mod record_tape;
pub mod replay;
