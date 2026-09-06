//! Native Rust support for Wii Remote devices exposed by Linux `hid-wiimote`.

mod backend;
pub(crate) mod decode;
pub(crate) mod model;

// Device and monitor implementations own the kernel-facing runtime state.
mod device;
mod monitor;
mod sys;

pub use decode::{Event, EventKind, EventType};
pub use device::{Interface, OpenError};
pub use model::{Axis3, Button, ButtonEvent, ButtonState, InterfaceMask, Timestamp};
pub use monitor::{Monitor, MonitorMode, MonitorPoll};

#[cfg(test)]
mod decode_tests;
