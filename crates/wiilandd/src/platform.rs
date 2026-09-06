//! System boundaries used by the reactor and deterministic runtime tests.
use crate::bridge::{BridgeAction, BridgeDevice};
use crate::signal::SignalPipe;
use crate::uinput::Backend;
use std::cell::Cell;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::time::Instant;
use wiiland_core::{Config, TraceFilter};
use wiiland_hid::{Event, Monitor, MonitorMode, MonitorPoll};
use wiiland_ipc::DeviceInfo;

/// Extra candidates allow unavailable devices to coexist with 32 active slots.
/// Overflow is an error, never a truncated authoritative removal snapshot.
pub const MAX_SNAPSHOT_DEVICES: usize = crate::runtime::MAX_DEVICES * 4;

fn collect_snapshot(
    mut next: impl FnMut() -> io::Result<Option<PathBuf>>,
) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    while let Some(path) = next()? {
        if paths.len() == MAX_SNAPSHOT_DEVICES {
            return Err(io::Error::from_raw_os_error(libc::EOVERFLOW));
        }
        paths.push(path);
    }
    Ok(paths)
}

/// One owned input/output session. All event processing stays on the reactor.
pub trait DeviceSession {
    fn path(&self) -> &Path;
    fn fd(&self) -> RawFd;
    fn info(&self) -> DeviceInfo;
    fn drain(&mut self, observer: &mut dyn FnMut(&Event)) -> Result<BridgeAction, i32>;
    fn set_capture(&mut self, enabled: bool) -> Result<(), i32>;
    fn pointer_active(&self) -> bool;
    fn tick_pointer(&mut self) -> Result<(), i32>;
    fn set_trace(
        &mut self,
        filter: TraceFilter,
        sequence: Rc<Cell<u64>>,
        sink: Box<dyn FnMut(&str)>,
    );
}

/// A snapshot request is asynchronous; at most one request is outstanding.
/// Implementations must not retain descriptors from the poll slice. Completed
/// snapshots must be complete, with at most MAX_SNAPSHOT_DEVICES paths.
pub trait RuntimePlatform<B: Backend + Clone> {
    type Device: DeviceSession;
    fn open_device(
        &mut self,
        path: &Path,
        config: &Config,
        backend: B,
        outputs: bool,
    ) -> Result<Self::Device, i32>;
    fn start_monitor(&mut self) -> io::Result<()>;
    fn monitor_fd(&self) -> Option<RawFd>;
    fn drain_monitor(&mut self, budget: usize) -> io::Result<bool>;
    fn request_snapshot(&mut self) -> io::Result<()>;
    fn take_snapshot(&mut self) -> Option<io::Result<Vec<PathBuf>>>;
    fn signal_fd(&self) -> RawFd;
    fn shutdown_requested(&self) -> bool;
    fn drain_signal(&mut self);
    fn now(&self) -> Instant;
    fn poll(&mut self, fds: &mut [libc::pollfd], timeout_ms: i32) -> io::Result<usize>;
}

/// Only enumeration runs in a worker; HID sessions and uinput remain local.
pub struct SystemPlatform {
    signal: SignalPipe,
    monitor: Option<Monitor>,
    requests: SyncSender<()>,
    snapshots: Receiver<io::Result<Vec<PathBuf>>>,
}

impl SystemPlatform {
    pub fn new() -> Result<Self, i32> {
        let signal = SignalPipe::install()?;
        let (requests, incoming) = mpsc::sync_channel(1);
        let (completed, snapshots) = mpsc::sync_channel(1);
        crate::signal::spawn_worker("wiiland-discovery", move || {
            for () in incoming {
                let snapshot = (|| {
                    let mut monitor = Monitor::new(MonitorMode::Enumerate)?;
                    collect_snapshot(|| monitor.poll())
                })();
                if completed.send(snapshot).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| -error.raw_os_error().unwrap_or(libc::EIO))?;
        Ok(Self {
            signal,
            monitor: None,
            requests,
            snapshots,
        })
    }
}

impl<B: Backend + Clone> RuntimePlatform<B> for SystemPlatform {
    type Device = BridgeDevice<B>;

    fn open_device(
        &mut self,
        path: &Path,
        config: &Config,
        backend: B,
        outputs: bool,
    ) -> Result<Self::Device, i32> {
        BridgeDevice::with_backend_outputs(path, config, backend, outputs)
    }
    fn start_monitor(&mut self) -> io::Result<()> {
        self.monitor = Some(Monitor::new(MonitorMode::Watch)?);
        Ok(())
    }
    fn monitor_fd(&self) -> Option<RawFd> {
        self.monitor
            .as_ref()
            .and_then(|monitor| monitor.fd().map(|fd| fd.as_raw_fd()))
    }
    fn drain_monitor(&mut self, budget: usize) -> io::Result<bool> {
        if let Some(monitor) = self.monitor.as_mut() {
            for _ in 0..budget {
                if matches!(monitor.poll_bounded(1)?, MonitorPoll::Empty) {
                    return Ok(false);
                }
            }
            return Ok(true);
        }
        Ok(false)
    }
    fn request_snapshot(&mut self) -> io::Result<()> {
        match self.requests.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => Ok(()),
            Err(TrySendError::Disconnected(())) => {
                Err(io::Error::other("discovery worker stopped"))
            }
        }
    }
    fn take_snapshot(&mut self) -> Option<io::Result<Vec<PathBuf>>> {
        match self.snapshots.try_recv() {
            Ok(snapshot) => Some(snapshot),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Err(io::Error::other("discovery worker stopped")))
            }
        }
    }
    fn signal_fd(&self) -> RawFd {
        self.signal.read_fd()
    }
    fn shutdown_requested(&self) -> bool {
        self.signal.requested()
    }
    fn drain_signal(&mut self) {
        let _ = self.signal.drain();
    }
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn poll(&mut self, fds: &mut [libc::pollfd], timeout_ms: i32) -> io::Result<usize> {
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_limit_never_presents_a_truncated_list_as_authoritative() {
        for count in [MAX_SNAPSHOT_DEVICES, MAX_SNAPSHOT_DEVICES + 1] {
            let mut paths = (0..count).map(|n| PathBuf::from(format!("/device/{n}")));
            let result = collect_snapshot(|| Ok(paths.next()));
            if count == MAX_SNAPSHOT_DEVICES {
                assert_eq!(result.unwrap().len(), count);
            } else {
                assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EOVERFLOW));
            }
        }
    }
}
