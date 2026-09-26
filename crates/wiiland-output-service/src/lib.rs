//! Windows user-mode output broker for the WiiLand virtual HID driver.
//!
//! The broker accepts only fixed-size reports described by the driver's v1
//! ABI. The service owns the driver handle and destroys every lease when its
//! pipe client exits, the console locks/logs off, or the machine suspends.

mod protocol;

pub use protocol::{MAX_REPORT_PAYLOAD, MAX_SLOTS, OutputLease, OutputReport, ReportId};

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::OutputClient;

#[cfg(not(windows))]
/// A non-Windows build can depend on the crate, but cannot connect to the
/// Windows output service.
pub struct OutputClient;

#[cfg(not(windows))]
impl OutputClient {
    /// Windows output is unavailable on this target.
    pub fn connect() -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "WiiLand output broker is available only on Windows",
        ))
    }

    /// Windows output is unavailable on this target.
    pub fn create(&mut self) -> std::io::Result<OutputLease> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "WiiLand output broker is available only on Windows",
        ))
    }

    /// Windows output is unavailable on this target.
    pub fn submit(&mut self, _lease: OutputLease, _report: &OutputReport) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "WiiLand output broker is available only on Windows",
        ))
    }

    /// Windows output is unavailable on this target.
    pub fn destroy(&mut self, _lease: OutputLease) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "WiiLand output broker is available only on Windows",
        ))
    }
}

/// Run the `WiiLandOutput` service entry point under the Windows Service
/// Control Manager.
#[cfg(windows)]
pub fn run_service() -> std::io::Result<()> {
    windows::service::run()
}

/// The service executable is supported only on Windows.
#[cfg(not(windows))]
pub fn run_service() -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "WiiLandOutput is a Windows service",
    ))
}
