//! Bounded background IPC subscription for interactive consumers.
use crate::{Client, ClientError, DeviceInfo, Notification, Status};
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, SyncSender, TryRecvError},
};
use std::time::Duration;

const EVENT_QUEUE: usize = 64;

#[derive(Debug)]
pub enum SessionEvent {
    Connected {
        status: Status,
        devices: Vec<DeviceInfo>,
    },
    Notification(Notification),
}

/// Owns a worker and its connection. Dropping cancels reads and releases capture
/// leases, without waiting on the UI thread. Queue overflow terminates explicitly.
pub struct Session {
    events: Receiver<SessionEvent>,
    finished: Receiver<Result<(), ClientError>>,
    cancelled: Arc<AtomicBool>,
    pending_event: RefCell<Option<SessionEvent>>,
    pending_result: RefCell<Option<Result<(), ClientError>>>,
}

impl Session {
    /// Empty selection captures all currently known devices; single selects the
    /// first device when the selector is empty. Otherwise use a syspath or ordinal.
    pub fn start(socket: Option<PathBuf>, selector: String, single: bool) -> Self {
        let (sender, events) = mpsc::sync_channel(EVENT_QUEUE);
        let (done, finished) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancelled);
        std::thread::spawn(move || {
            let result = run(socket, selector, single, &sender, &stop);
            let _ = done.send(result);
        });
        Self {
            events,
            finished,
            cancelled,
            pending_event: RefCell::new(None),
            pending_result: RefCell::new(None),
        }
    }
    pub fn try_recv(&self) -> Result<SessionEvent, TryRecvError> {
        self.pending_event
            .borrow_mut()
            .take()
            .map(Ok)
            .unwrap_or_else(|| self.events.try_recv())
    }
    /// Completion becomes available only after all queued events are delivered.
    pub fn try_finish(&self) -> Result<Result<(), ClientError>, TryRecvError> {
        let result = match self.pending_result.borrow_mut().take() {
            Some(result) => result,
            None => self.finished.try_recv()?,
        };
        if self.pending_event.borrow().is_some() {
            self.pending_result.replace(Some(result));
            return Err(TryRecvError::Empty);
        }
        // Receiving completion happens after the worker's final event send.
        if let Ok(event) = self.events.try_recv() {
            self.pending_event.replace(Some(event));
            self.pending_result.replace(Some(result));
            return Err(TryRecvError::Empty);
        }
        Ok(result)
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn send(sender: &SyncSender<SessionEvent>, event: SessionEvent) -> Result<(), ClientError> {
    sender
        .try_send(event)
        .map_err(|_| ClientError::SessionBacklogExceeded { limit: EVENT_QUEUE })
}

fn run(
    socket: Option<PathBuf>,
    selector: String,
    single: bool,
    sender: &SyncSender<SessionEvent>,
    stop: &AtomicBool,
) -> Result<(), ClientError> {
    let mut client = match socket {
        Some(path) => Client::connect(path)?,
        None => Client::connect_default()?,
    };
    client.set_read_timeout(Some(Duration::from_secs(2)))?;
    client.set_write_timeout(Some(Duration::from_secs(2)))?;
    let status = client.status()?;
    let mut devices = select_devices(client.devices()?, &selector, single)?;
    client.subscribe()?;
    for device in &mut devices {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        *device = client.start_capture(&device.syspath)?;
    }
    let selected: Vec<String> = devices
        .iter()
        .map(|device| device.syspath.clone())
        .collect();
    send(sender, SessionEvent::Connected { status, devices })?;
    client.set_read_timeout(Some(Duration::from_millis(100)))?;
    while !stop.load(Ordering::Relaxed) {
        match client.next_event() {
            Ok(event) => {
                let path = match &event {
                    Notification::Input { syspath, .. }
                    | Notification::DeviceRemoved { syspath, .. } => Some(syspath),
                    Notification::DeviceAdded { device, .. } => Some(&device.syspath),
                    _ => None,
                };
                if path.is_none_or(|path| selected.contains(path)) {
                    send(sender, SessionEvent::Notification(event))?;
                }
            }
            Err(ClientError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error),
        }
    }
    // Closing the socket releases leases, including after partial setup failures.
    Ok(())
}

pub fn select_devices(
    devices: Vec<DeviceInfo>,
    selector: &str,
    single: bool,
) -> Result<Vec<DeviceInfo>, ClientError> {
    let invalid = || {
        ClientError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "no daemon device matches the selector",
        ))
    };
    if selector.is_empty() {
        if devices.is_empty() {
            return Err(invalid());
        }
        return Ok(if single {
            devices.into_iter().take(1).collect()
        } else {
            devices
        });
    }
    if selector.bytes().all(|byte| byte.is_ascii_digit()) {
        let index = selector
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .ok_or_else(invalid)?;
        return devices
            .into_iter()
            .nth(index)
            .map(|device| vec![device])
            .ok_or_else(invalid);
    }
    devices
        .into_iter()
        .find(|device| device.syspath == selector)
        .map(|device| vec![device])
        .ok_or_else(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_queue_fails_explicitly_and_completion_waits_for_last_event() {
        let (sender, events) = mpsc::sync_channel(EVENT_QUEUE);
        let (done, finished) = mpsc::sync_channel(1);
        let session = Session {
            events,
            finished,
            cancelled: Arc::new(AtomicBool::new(false)),
            pending_event: RefCell::new(None),
            pending_result: RefCell::new(None),
        };
        for _ in 0..EVENT_QUEUE {
            send(
                &sender,
                SessionEvent::Notification(Notification::Unsupported),
            )
            .unwrap();
        }
        assert!(matches!(
            send(
                &sender,
                SessionEvent::Notification(Notification::Unsupported)
            ),
            Err(ClientError::SessionBacklogExceeded { .. })
        ));
        done.send(Ok(())).unwrap();
        assert!(matches!(session.try_finish(), Err(TryRecvError::Empty)));
        for _ in 0..EVENT_QUEUE {
            assert!(session.try_recv().is_ok());
        }
        session.try_finish().unwrap().unwrap();
    }
}
