//! Live application services: typed results, worker-owned calibration windows.
use crate::model::ConfigModel;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
};
use std::time::{Duration, Instant};
use wiiland_core::{SensorCalibration, TraceFilter, calibration::CalibrationStats};
use wiiland_ipc::{
    CaptureConnection, ClientError, DeviceInfo, Diagnostics, InputPayload, Notification, Session,
    SessionEvent, Status,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CalibrationResult {
    pub accel: Option<SensorCalibration>,
    pub motion_plus: Option<SensorCalibration>,
}

#[derive(Debug)]
pub enum CaptureResult {
    TraceStopped,
    Calibrated(CalibrationResult),
}

enum CaptureKind {
    Trace {
        session: Box<Session>,
        filter: TraceFilter,
        failed: Option<String>,
    },
    Calibration(CalibrationWorker),
}
pub struct Capture(CaptureKind);
impl Capture {
    pub fn start(selector: String, filter: TraceFilter, duration: Option<Duration>) -> Self {
        Self::with_socket(None, selector, filter, duration)
    }
    fn with_socket(
        socket: Option<PathBuf>,
        selector: String,
        filter: TraceFilter,
        duration: Option<Duration>,
    ) -> Self {
        Self(match duration {
            Some(duration) => {
                CaptureKind::Calibration(CalibrationWorker::start(socket, selector, duration))
            }
            None => CaptureKind::Trace {
                session: Box::new(Session::start(socket, selector, false)),
                filter,
                failed: None,
            },
        })
    }
    pub fn cancel(&self) {
        match &self.0 {
            CaptureKind::Trace { session, .. } => session.cancel(),
            CaptureKind::Calibration(worker) => worker.cancel(),
        }
    }
    pub fn poll(&mut self, model: &mut ConfigModel) -> Option<Result<CaptureResult, String>> {
        match &mut self.0 {
            CaptureKind::Calibration(worker) => {
                if let Ok((status, devices)) = worker.connected.try_recv() {
                    connected(model, &status, &devices);
                }
                match worker.result.try_recv() {
                    Ok(result) => Some(result.map(CaptureResult::Calibrated)),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => {
                        Some(Err("Calibration worker stopped without a result".into()))
                    }
                }
            }
            CaptureKind::Trace {
                session,
                filter,
                failed,
            } => {
                for _ in 0..256 {
                    let Ok(event) = session.try_recv() else {
                        break;
                    };
                    match event {
                        SessionEvent::Connected { status, devices } => {
                            connected(model, &status, &devices)
                        }
                        SessionEvent::Notification(Notification::Input {
                            sequence,
                            syspath,
                            timestamp,
                            payload,
                        }) => {
                            if filter.matches(payload.event_code()) {
                                model.append_output(&format!(
                                    "seq={sequence} time={}.{:06} device={syspath} {payload:?}\n",
                                    timestamp.seconds, timestamp.micros
                                ));
                            }
                        }
                        SessionEvent::Notification(Notification::DeviceRemoved {
                            syspath, ..
                        }) => {
                            *failed = Some(format!("Device disconnected: {syspath}"));
                            session.cancel();
                        }
                        _ => {}
                    }
                }
                session.try_finish().ok().map(|result| {
                    result.map_err(|error| format!("Daemon capture: {error}"))?;
                    if let Some(error) = failed.take() {
                        return Err(error);
                    }
                    Ok(CaptureResult::TraceStopped)
                })
            }
        }
    }
}
fn connected(model: &mut ConfigModel, status: &Status, devices: &[DeviceInfo]) {
    model.append_output(&format!(
        "Connected to wiilandd {} (pid {}), {} capture device(s)\n",
        status.daemon_version,
        status.pid,
        devices.len()
    ));
}

struct CalibrationWorker {
    cancelled: Arc<AtomicBool>,
    connected: Receiver<(Status, Vec<DeviceInfo>)>,
    result: Receiver<Result<CalibrationResult, String>>,
}
impl CalibrationWorker {
    fn start(socket: Option<PathBuf>, selector: String, duration: Duration) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancelled);
        let (connected, connection) = mpsc::sync_channel(1);
        let (done, result) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = (|| {
                let mut capture =
                    CaptureConnection::connect_cancellable(socket, &selector, true, || {
                        stop.load(Ordering::Relaxed)
                    })
                    .map_err(|error| format!("Daemon capture: {error}"))?;
                let _ = connected.send((capture.status().clone(), capture.devices().to_vec()));
                let deadline = Instant::now() + duration;
                let mut samples = CalibrationSamples::default();
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return Err("Capture cancelled".into());
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    capture
                        .set_read_timeout(Some(remaining.min(Duration::from_millis(50))))
                        .map_err(|error| error.to_string())?;
                    match capture.next_event() {
                        Ok(Some(event)) => samples.observe(event)?,
                        Ok(None) => {}
                        Err(ClientError::Io(error))
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(error) => return Err(format!("Daemon capture: {error}")),
                    }
                }
                // The connection and its leases are dropped before result delivery.
                samples.finish()
            })();
            let _ = done.send(result);
        });
        Self {
            cancelled,
            connected: connection,
            result,
        }
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}
impl Drop for CalibrationWorker {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Default)]
struct CalibrationSamples {
    accel: CalibrationStats,
    motion: CalibrationStats,
}
impl CalibrationSamples {
    fn observe(&mut self, event: Notification) -> Result<(), String> {
        match event {
            Notification::Input { payload, .. } => match payload {
                InputPayload::Accel(v) => self.accel.add([v.x, v.y, v.z]),
                InputPayload::MotionPlus(v) => self.motion.add([v.x, v.y, v.z]),
                InputPayload::Watch | InputPayload::Gone => {
                    return Err(
                        "Device interfaces changed during calibration; capture again".into(),
                    );
                }
                _ => {}
            },
            Notification::DeviceRemoved { syspath, .. } => {
                return Err(format!("Device disconnected: {syspath}"));
            }
            _ => {}
        }
        Ok(())
    }
    fn finish(self) -> Result<CalibrationResult, String> {
        let result = CalibrationResult {
            accel: self.accel.finish(),
            motion_plus: self.motion.finish(),
        };
        if result.accel.is_none() && result.motion_plus.is_none() {
            Err("No stable sensor samples; keep the controller still and capture again".into())
        } else {
            Ok(result)
        }
    }
}

pub enum QueryResult {
    Status {
        status: Status,
        diagnostics: Diagnostics,
        config: String,
    },
    Devices(Vec<DeviceInfo>),
}
pub struct Query(Receiver<Result<QueryResult, String>>, bool);
impl Query {
    pub fn start(devices: bool) -> Self {
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = (|| {
                let mut client = wiiland_ipc::Client::connect_default()?;
                client.set_read_timeout(Some(Duration::from_secs(2)))?;
                client.set_write_timeout(Some(Duration::from_secs(2)))?;
                if devices {
                    Ok(QueryResult::Devices(client.devices()?))
                } else {
                    Ok(QueryResult::Status {
                        status: client.status()?,
                        diagnostics: client.diagnostics()?,
                        config: client.config()?,
                    })
                }
            })()
            .map_err(|error: ClientError| {
                format!(
                    "Cannot query running daemon: {error}. Start or upgrade the service and retry."
                )
            });
            let _ = sender.send(result);
        });
        Self(receiver, devices)
    }
    pub fn is_status(&self) -> bool {
        !self.1
    }
    pub fn poll(&self) -> Option<Result<QueryResult, String>> {
        match self.0.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Err("Daemon query worker stopped without a result".into()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use wiiland_ipc::{
        Axis3, Command, DeviceInfo, Profile, Request, ResponseResult, ServerMessage, Status,
        Timestamp,
    };

    fn calibration_capture(interrupted: bool) -> Result<CaptureResult, String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let device = DeviceInfo {
                syspath: "/sys/test".into(),
                profile: Profile::Desktop,
                opened_interfaces: 0x103,
                pending_interfaces: 0,
                gamepad_output: false,
                desktop_output: true,
            };
            loop {
                let mut bytes = Vec::new();
                reader.read_until(b'\n', &mut bytes).unwrap();
                let request: Request = wiiland_ipc::decode_frame(&bytes).unwrap();
                let capture = matches!(request.command, Command::StartCapture { .. });
                let result = match request.command {
                    Command::Hello { .. } => ResponseResult::Hello {
                        major: 1,
                        minor: 1,
                        daemon_version: "test".into(),
                    },
                    Command::Status => ResponseResult::Status(Status {
                        daemon_version: "test".into(),
                        pid: 1,
                        device_count: 1,
                        dry_run: true,
                        socket_path: String::new(),
                    }),
                    Command::Devices => ResponseResult::Devices(vec![device.clone()]),
                    Command::Subscribe { .. } => ResponseResult::Subscribed,
                    Command::StartCapture { .. } => ResponseResult::CaptureStarted(device.clone()),
                    command => panic!("unexpected request: {command:?}"),
                };
                stream
                    .write_all(
                        &wiiland_ipc::encode_frame(&ServerMessage::Response {
                            id: request.id,
                            result,
                        })
                        .unwrap(),
                    )
                    .unwrap();
                if capture {
                    break;
                }
            }
            for sequence in 0..1024 {
                let axis = Axis3 {
                    x: 10,
                    y: 20,
                    z: 30,
                };
                let payload = if sequence % 2 == 0 {
                    InputPayload::Accel(axis)
                } else {
                    InputPayload::MotionPlus(axis)
                };
                let notification = Notification::Input {
                    sequence,
                    syspath: device.syspath.clone(),
                    timestamp: Timestamp {
                        seconds: 0,
                        micros: sequence as u32,
                    },
                    payload,
                };
                stream
                    .write_all(
                        &wiiland_ipc::encode_frame(&ServerMessage::Notification(notification))
                            .unwrap(),
                    )
                    .unwrap();
            }
            if interrupted {
                let notification = Notification::DeviceRemoved {
                    sequence: 1024,
                    syspath: device.syspath,
                    reason: wiiland_ipc::RemovalReason::Gone,
                };
                stream
                    .write_all(
                        &wiiland_ipc::encode_frame(&ServerMessage::Notification(notification))
                            .unwrap(),
                    )
                    .unwrap();
            }
            let mut bytes = Vec::new();
            assert_eq!(reader.read_until(b'\n', &mut bytes).unwrap(), 0);
        });
        let mut capture = Capture::with_socket(
            Some(path),
            "1".into(),
            TraceFilter::All,
            Some(Duration::from_millis(200)),
        );
        // Wait for the server to observe lease release without polling the UI.
        // More samples than the UI queue can hold must still calibrate successfully.
        server.join().unwrap();
        let mut model = ConfigModel::new(dir.path().join("config"));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(result) = capture.poll(&mut model) {
                break result;
            }
            assert!(Instant::now() < deadline, "capture did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn daemon_samples_produce_independent_complete_calibration_triples() {
        let result = calibration_capture(false);
        let CaptureResult::Calibrated(result) = result.unwrap() else {
            panic!("expected calibration");
        };
        assert_eq!(result.accel.unwrap().x, 10);
        assert_eq!(result.motion_plus.unwrap().z, 30);
    }

    #[test]
    fn device_loss_rejects_even_a_previously_stable_capture() {
        let result = calibration_capture(true);
        assert!(result.unwrap_err().contains("disconnected"));
    }
}
