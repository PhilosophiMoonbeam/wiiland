//! Native Rust support for Wii Remote HID devices on Linux and Windows.

#[cfg(not(windows))]
mod backend;
pub(crate) mod decode;
pub(crate) mod model;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_pairing;

#[cfg(not(windows))]
mod device;
#[cfg(not(windows))]
mod monitor;
#[cfg(not(windows))]
mod sys;

pub use decode::{Event, EventKind, EventType};
#[cfg(not(windows))]
pub use device::{Interface, OpenError};
pub use model::{Axis3, Button, ButtonEvent, ButtonState, InterfaceMask, Timestamp};
#[cfg(not(windows))]
pub use monitor::{Monitor, MonitorMode, MonitorPoll};
#[cfg(windows)]
pub use windows::{Interface, Monitor, MonitorMode, MonitorPoll, OpenError};
#[cfg(windows)]
pub use windows_pairing::{
    BluetoothAddress, Device, PairingCancellation, PairingError, PairingMethod, PairingResult,
    Radio, enumerate_devices, enumerate_radios, pair_device,
};

#[cfg(all(test, not(windows)))]
mod decode_tests;
