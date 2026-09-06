//! Blocking, connection-owned sensor capture for worker-side consumers.
use crate::{Client, ClientError, DeviceInfo, Notification, Status, select_devices};
use std::path::PathBuf;
use std::time::Duration;

/// A snapshot of selected devices and their capture leases. Dropping the
/// connection releases every lease, including after a partial setup failure.
/// Use this on a worker when every sample must be processed independently of UI
/// scheduling; [`crate::Session`] provides bounded delivery to a UI instead.
pub struct CaptureConnection {
    client: Client,
    status: Status,
    devices: Vec<DeviceInfo>,
}

impl CaptureConnection {
    pub fn connect(
        socket: Option<PathBuf>,
        selector: &str,
        single: bool,
    ) -> Result<Self, ClientError> {
        Self::connect_cancellable(socket, selector, single, || false)
    }

    /// Check cancellation between setup commands. Individual commands retain
    /// the two-second I/O timeout; cancellation never waits on a UI thread.
    pub fn connect_cancellable(
        socket: Option<PathBuf>,
        selector: &str,
        single: bool,
        cancelled: impl Fn() -> bool,
    ) -> Result<Self, ClientError> {
        let check = || {
            if cancelled() {
                Err(ClientError::Io(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "capture cancelled",
                )))
            } else {
                Ok(())
            }
        };
        check()?;
        let mut client = match socket {
            Some(path) => Client::connect(path)?,
            None => Client::connect_default()?,
        };
        client.set_read_timeout(Some(Duration::from_secs(2)))?;
        client.set_write_timeout(Some(Duration::from_secs(2)))?;
        check()?;
        let status = client.status()?;
        check()?;
        let mut devices = select_devices(client.devices()?, selector, single)?;
        check()?;
        client.subscribe()?;
        for device in &mut devices {
            check()?;
            *device = client.start_capture(&device.syspath)?;
        }
        check()?;
        client.set_read_timeout(Some(Duration::from_millis(100)))?;
        Ok(Self {
            client,
            status,
            devices,
        })
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn devices(&self) -> &[DeviceInfo] {
        &self.devices
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), ClientError> {
        self.client.set_read_timeout(timeout)
    }

    /// Read at most one socket chunk. Partial frames and unselected devices
    /// return `None`, so the caller can check its deadline between reads.
    pub fn next_event(&mut self) -> Result<Option<Notification>, ClientError> {
        let Some(event) = self.client.poll_event()? else {
            return Ok(None);
        };
        let path = match &event {
            Notification::Input { syspath, .. } | Notification::DeviceRemoved { syspath, .. } => {
                Some(syspath)
            }
            Notification::DeviceAdded { device, .. } => Some(&device.syspath),
            _ => None,
        };
        Ok(path
            .is_none_or(|path| self.devices.iter().any(|device| &device.syspath == path))
            .then_some(event))
    }
}
