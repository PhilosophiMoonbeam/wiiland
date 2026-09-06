//! Application-side live capture. Calibration is computed from daemon samples.
use crate::{model::ConfigModel, process::ProcessResult};
use std::time::{Duration, Instant};
use wiiland_core::{TraceFilter, calibration::CalibrationStats};
use wiiland_ipc::{InputPayload, Notification, Session, SessionEvent};

pub struct Capture {
    session: Session,
    filter: TraceFilter,
    duration: Option<Duration>,
    started: Option<Instant>,
    accel: CalibrationStats,
    motion: CalibrationStats,
    failed: Option<String>,
}
impl Capture {
    pub fn start(selector: String, filter: TraceFilter, duration: Option<Duration>) -> Self {
        Self::with_session(
            Session::start(None, selector, duration.is_some()),
            filter,
            duration,
        )
    }
    fn with_session(session: Session, filter: TraceFilter, duration: Option<Duration>) -> Self {
        Self {
            session,
            filter,
            duration,
            started: None,
            accel: CalibrationStats::new(),
            motion: CalibrationStats::new(),
            failed: None,
        }
    }
    pub fn cancel(&self) {
        self.session.cancel();
    }
    pub fn poll(&mut self, model: &mut ConfigModel) -> Option<ProcessResult> {
        // Both the IPC worker and this per-frame drain have fixed bounds.
        for _ in 0..256 {
            let Ok(event) = self.session.try_recv() else {
                break;
            };
            match event {
                SessionEvent::Connected { status, devices } => {
                    model.append_output(&format!(
                        "Connected to wiilandd {} (pid {}), {} capture device(s)\n",
                        status.daemon_version,
                        status.pid,
                        devices.len()
                    ));
                    self.started = Some(Instant::now());
                }
                SessionEvent::Notification(Notification::Input {
                    sequence,
                    syspath,
                    timestamp,
                    payload,
                }) => {
                    if self.duration.is_some() {
                        if matches!(payload, InputPayload::Watch | InputPayload::Gone) {
                            self.failed = Some(
                                "Device interfaces changed during calibration; capture again"
                                    .into(),
                            );
                            self.session.cancel();
                        }
                        match payload {
                            InputPayload::Accel(v) => self.accel.add([v.x, v.y, v.z]),
                            InputPayload::MotionPlus(v) => self.motion.add([v.x, v.y, v.z]),
                            _ => {}
                        }
                    } else if self.filter.matches(payload.event_code()) {
                        model.append_output(&format!(
                            "seq={sequence} time={}.{:06} device={syspath} {payload:?}\n",
                            timestamp.seconds, timestamp.micros
                        ));
                    }
                }
                SessionEvent::Notification(Notification::DeviceRemoved { syspath, .. }) => {
                    self.failed = Some(format!("Device disconnected: {syspath}"));
                    self.session.cancel();
                }
                _ => {}
            }
        }
        if self
            .duration
            .zip(self.started)
            .is_some_and(|(duration, start)| start.elapsed() >= duration)
        {
            self.session.cancel();
        }
        let result = self.session.try_finish().ok()?;
        if let Err(error) = result {
            return Some(ProcessResult::unavailable(format!(
                "Daemon capture: {error}"
            )));
        }
        if let Some(error) = self.failed.take() {
            return Some(ProcessResult::unavailable(error));
        }
        let mut output = String::new();
        if self.duration.is_some() {
            for (name, stats) in [
                ("aim-accel-zero", &self.accel),
                ("aim-motion-plus-bias", &self.motion),
            ] {
                if let Some(value) = stats.finish() {
                    output.push_str(&format!(
                        "{name}-x={}\n{name}-y={}\n{name}-z={}\n",
                        value.x, value.y, value.z
                    ));
                }
            }
            if output.is_empty() {
                return Some(ProcessResult::unavailable(
                    "No stable sensor samples; keep the controller still and capture again",
                ));
            }
        }
        Some(ProcessResult {
            success: true,
            code: Some(0),
            stdout: output.into_bytes(),
            stderr: Vec::new(),
            error: None,
        })
    }
}

pub struct Query(std::sync::mpsc::Receiver<Result<String, String>>, bool);
impl Query {
    pub fn start(devices: bool) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = (|| {
                let mut client = wiiland_ipc::Client::connect_default()?;
                client.set_read_timeout(Some(Duration::from_secs(2)))?;
                client.set_write_timeout(Some(Duration::from_secs(2)))?;
                if devices {
                    Ok(client.devices()?.iter().enumerate().map(|(index, device)| format!("{}\t{}\t{:?}\n", index + 1, device.syspath, device.profile)).collect())
                } else {
                    let status = client.status()?;
                    let health = client.diagnostics()?;
                    Ok(format!("wiilandd {} (pid {}): {} device(s)\ntrace drops={} lifecycle drops={} max pointer lateness={}us max dispatch={}us\nRunning configuration:\n{}", status.daemon_version, status.pid, status.device_count, health.trace_records_dropped, health.lifecycle_records_dropped, health.max_pointer_lateness_us, health.max_dispatch_duration_us, client.config()?))
                }
            })().map_err(|error: wiiland_ipc::ClientError| format!("Cannot query running daemon: {error}. Start or upgrade the service and retry."));
            let _ = sender.send(result);
        });
        Self(receiver, devices)
    }
    pub fn is_status(&self) -> bool {
        !self.1
    }
    pub fn poll(&self) -> Option<Result<String, String>> {
        self.0.try_recv().ok()
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

    fn calibration_capture(interrupted: bool) -> ProcessResult {
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
            for sequence in 0..32 {
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
                    sequence: 32,
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
        let mut capture = Capture::with_session(
            Session::start(Some(path), "1".into(), true),
            TraceFilter::All,
            Some(Duration::from_millis(30)),
        );
        let mut model = ConfigModel::new(dir.path().join("config"));
        let deadline = Instant::now() + Duration::from_secs(3);
        let result = loop {
            if let Some(result) = capture.poll(&mut model) {
                break result;
            }
            assert!(Instant::now() < deadline, "capture did not finish");
            std::thread::sleep(Duration::from_millis(5));
        };
        server.join().unwrap();
        result
    }

    #[test]
    fn daemon_samples_produce_independent_complete_calibration_triples() {
        let result = calibration_capture(false);
        assert!(result.success, "{:?}", result.error);
        let parsed = wiiland_core::Config::parse_bytes("capture", &result.stdout).unwrap();
        assert_eq!(parsed.aim_accel_zero.unwrap().x, 10);
        assert_eq!(parsed.aim_motion_plus_bias.unwrap().z, 30);
    }

    #[test]
    fn device_loss_rejects_even_a_previously_stable_capture() {
        let result = calibration_capture(true);
        assert!(!result.success);
        assert!(result.stdout.is_empty());
    }
}
