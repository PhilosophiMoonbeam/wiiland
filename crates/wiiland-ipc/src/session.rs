//! Bounded background IPC subscription for interactive consumers.
use crate::{CaptureConnection, ClientError, DeviceInfo, InputPayload, Notification, Status};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, TryRecvError},
};

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
    events: Arc<EventQueue>,
    finished: Receiver<Result<(), ClientError>>,
    cancelled: Arc<AtomicBool>,
    pending_event: RefCell<Option<SessionEvent>>,
    pending_result: RefCell<Option<Result<(), ClientError>>>,
}

impl Session {
    /// Empty selection captures all currently known devices; single selects the
    /// first device when the selector is empty. Otherwise use a syspath or ordinal.
    pub fn start(socket: Option<PathBuf>, selector: String, single: bool) -> Self {
        Self::start_with_policy(socket, selector, single, DeliveryPolicy::Strict)
    }

    /// Keep recent sensor values when the UI falls behind. Button transitions
    /// and lifecycle events are never coalesced; their overflow still fails.
    pub fn start_visualization(socket: Option<PathBuf>, selector: String, single: bool) -> Self {
        Self::start_with_policy(socket, selector, single, DeliveryPolicy::LatestSensors)
    }

    fn start_with_policy(
        socket: Option<PathBuf>,
        selector: String,
        single: bool,
        policy: DeliveryPolicy,
    ) -> Self {
        let events = Arc::new(EventQueue::new(policy));
        let sender = Arc::clone(&events);
        let (done, finished) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancelled);
        std::thread::spawn(move || {
            let result = run(socket, selector, single, &sender, &stop);
            sender.closed.store(true, Ordering::Release);
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
    /// Number of sensor samples replaced or evicted for a visualization.
    pub fn coalesced_samples(&self) -> u64 {
        self.events.coalesced.load(Ordering::Relaxed)
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

#[derive(Clone, Copy)]
enum DeliveryPolicy {
    Strict,
    LatestSensors,
}

struct EventQueue {
    events: Mutex<VecDeque<SessionEvent>>,
    policy: DeliveryPolicy,
    coalesced: AtomicU64,
    closed: AtomicBool,
}
impl EventQueue {
    fn new(policy: DeliveryPolicy) -> Self {
        Self {
            events: Mutex::new(VecDeque::with_capacity(EVENT_QUEUE)),
            policy,
            coalesced: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        }
    }
    fn send(&self, event: SessionEvent) -> Result<(), ClientError> {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if events.len() == EVENT_QUEUE {
            if matches!(self.policy, DeliveryPolicy::LatestSensors) {
                // Prefer replacing this sensor's previous value. Otherwise
                // evict the oldest sensor, preserving every control transition.
                let same = sensor_identity(&event).and_then(|identity| {
                    events
                        .iter()
                        .position(|old| sensor_identity(old) == Some(identity))
                });
                if let Some(index) =
                    same.or_else(|| events.iter().position(|old| sensor_identity(old).is_some()))
                {
                    events.remove(index);
                    self.coalesced.fetch_add(1, Ordering::Relaxed);
                } else if sensor_identity(&event).is_some() {
                    // A queue consisting entirely of controls takes priority
                    // over this new visualization sample.
                    self.coalesced.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
            }
            if events.len() == EVENT_QUEUE {
                return Err(ClientError::SessionBacklogExceeded { limit: EVENT_QUEUE });
            }
        }
        events.push_back(event);
        Ok(())
    }
    fn try_recv(&self) -> Result<SessionEvent, TryRecvError> {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        events.pop_front().ok_or_else(|| {
            if self.closed.load(Ordering::Acquire) {
                TryRecvError::Disconnected
            } else {
                TryRecvError::Empty
            }
        })
    }
}

fn sensor_identity(event: &SessionEvent) -> Option<(&str, u32)> {
    let SessionEvent::Notification(Notification::Input {
        syspath, payload, ..
    }) = event
    else {
        return None;
    };
    matches!(
        payload,
        InputPayload::Accel(_)
            | InputPayload::Ir(_)
            | InputPayload::BalanceBoard(_)
            | InputPayload::MotionPlus(_)
            | InputPayload::ProControllerMove(_)
            | InputPayload::ClassicControllerMove(_)
            | InputPayload::NunchukMove(_)
            | InputPayload::DrumsMove(_)
            | InputPayload::GuitarMove(_)
    )
    .then(|| (syspath.as_str(), payload.event_code()))
}

fn run(
    socket: Option<PathBuf>,
    selector: String,
    single: bool,
    sender: &EventQueue,
    stop: &AtomicBool,
) -> Result<(), ClientError> {
    let connection = CaptureConnection::connect_cancellable(socket, &selector, single, || {
        stop.load(Ordering::Relaxed)
    });
    let mut connection = match connection {
        Err(_) if stop.load(Ordering::Relaxed) => return Ok(()),
        result => result?,
    };
    sender.send(SessionEvent::Connected {
        status: connection.status().clone(),
        devices: connection.devices().to_vec(),
    })?;
    while !stop.load(Ordering::Relaxed) {
        match connection.next_event() {
            Ok(Some(event)) => sender.send(SessionEvent::Notification(event))?,
            Ok(None) => {}
            Err(ClientError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error),
        }
    }
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

    fn sample(sequence: u64, key: Option<u32>) -> SessionEvent {
        SessionEvent::Notification(Notification::Input {
            sequence,
            syspath: "/sys/test".into(),
            timestamp: crate::Timestamp {
                seconds: 0,
                micros: 0,
            },
            payload: key.map_or_else(
                || {
                    InputPayload::Accel(crate::Axis3 {
                        x: sequence as i32,
                        y: 0,
                        z: 0,
                    })
                },
                |state| InputPayload::Key(crate::ButtonEvent { code: 4, state }),
            ),
        })
    }

    #[test]
    fn stalled_visualization_preserves_controls_and_latest_sensor_in_wire_order() {
        let queue = EventQueue::new(DeliveryPolicy::LatestSensors);
        queue.send(sample(0, Some(1))).unwrap();
        for sequence in 1..=2000 {
            queue.send(sample(sequence, None)).unwrap();
        }
        queue.send(sample(2001, Some(0))).unwrap();
        queue
            .send(SessionEvent::Notification(Notification::DeviceRemoved {
                sequence: 2002,
                syspath: "/sys/test".into(),
                reason: crate::RemovalReason::Gone,
            }))
            .unwrap();
        assert_eq!(queue.events.lock().unwrap().len(), EVENT_QUEUE);
        assert_eq!(
            queue.coalesced.load(Ordering::Relaxed),
            2003 - EVENT_QUEUE as u64
        );
        let mut sequences = Vec::new();
        let mut buttons = Vec::new();
        while let Ok(event) = queue.try_recv() {
            match event {
                SessionEvent::Notification(Notification::Input {
                    sequence, payload, ..
                }) => {
                    sequences.push(sequence);
                    if let InputPayload::Key(button) = payload {
                        buttons.push(button.state);
                    }
                }
                SessionEvent::Notification(Notification::DeviceRemoved { sequence, .. }) => {
                    sequences.push(sequence)
                }
                _ => panic!("unexpected event"),
            }
        }
        assert_eq!(buttons, [1, 0]);
        assert!(sequences.contains(&2000));
        assert_eq!(sequences.last(), Some(&2002));
        assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn visualization_discards_samples_but_fails_explicitly_when_controls_overflow() {
        let queue = EventQueue::new(DeliveryPolicy::LatestSensors);
        for sequence in 0..EVENT_QUEUE as u64 {
            queue.send(sample(sequence, Some(1))).unwrap();
        }
        queue.send(sample(1000, None)).unwrap();
        assert_eq!(queue.coalesced.load(Ordering::Relaxed), 1);
        assert!(matches!(
            queue.send(sample(1001, Some(0))),
            Err(ClientError::SessionBacklogExceeded { .. })
        ));
        assert_eq!(queue.events.lock().unwrap().len(), EVENT_QUEUE);
    }
    #[test]
    fn full_queue_fails_explicitly_and_completion_waits_for_last_event() {
        let events = Arc::new(EventQueue::new(DeliveryPolicy::Strict));
        let sender = Arc::clone(&events);
        let (done, finished) = mpsc::sync_channel(1);
        let session = Session {
            events,
            finished,
            cancelled: Arc::new(AtomicBool::new(false)),
            pending_event: RefCell::new(None),
            pending_result: RefCell::new(None),
        };
        for _ in 0..EVENT_QUEUE {
            sender
                .send(SessionEvent::Notification(Notification::Unsupported))
                .unwrap();
        }
        assert!(matches!(
            sender.send(SessionEvent::Notification(Notification::Unsupported)),
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
