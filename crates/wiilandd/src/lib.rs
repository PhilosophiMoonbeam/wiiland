//! WiiLand's display-neutral input daemon.
//!
//! The command layer owns parsing, diagnostics, and hardware-free contracts;
//! runtime ownership stays in the sibling modules so the command paths remain
//! useful on machines without a Wii Remote or uinput.

pub mod bridge;
pub mod cli;
pub mod commands;
mod diagnostics;
mod ipc;
pub mod report;
pub mod runtime;
pub mod signal;
pub mod uinput;
pub use cli::{Action, Cli, CliError, IpcMode, Pass1, run};
