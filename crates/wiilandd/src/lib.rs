//! WiiLand's display-neutral input daemon.
//!
//! The command layer owns parsing, diagnostics, and hardware-free contracts;
//! runtime ownership stays in the sibling modules so the command paths remain
//! useful on machines without a Wii Remote or uinput.

pub mod bridge;
pub mod cli;
#[cfg(unix)]
pub mod commands;
#[cfg(unix)]
mod diagnostics;
mod ipc;
#[cfg(unix)]
pub mod platform;
pub mod report;
#[cfg(unix)]
pub mod runtime;
#[cfg(unix)]
pub mod signal;
#[cfg(unix)]
pub mod uinput;
#[cfg(windows)]
pub(crate) mod windows_output_worker;
#[cfg(windows)]
pub(crate) mod windows_runtime;
pub use cli::{Action, Cli, CliError, IpcMode, Pass1, run};
