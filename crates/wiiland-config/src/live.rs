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
    #[cfg(windows)]
    Stopped,
}
pub struct Query(Receiver<Result<QueryResult, String>>, bool);
impl Query {
    pub fn start(devices: bool) -> Self {
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = (|| {
                let mut client = match wiiland_ipc::Client::connect_default() {
                    Ok(client) => client,
                    Err(error) => {
                        #[cfg(windows)]
                        if !devices
                            && windows_lifecycle::endpoint_absent(&error)
                            && matches!(windows_lifecycle::stop_event_present(), Ok(false))
                        {
                            return Ok(QueryResult::Stopped);
                        }
                        return Err(error);
                    }
                };
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
                #[cfg(windows)]
                {
                    format!(
                        "Cannot query the running WiiLand daemon: {error}. Start the per-user daemon and retry."
                    )
                }
                #[cfg(not(windows))]
                {
                    format!(
                        "Cannot query running daemon: {error}. Start or upgrade the service and retry."
                    )
                }
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

#[cfg(windows)]
mod windows_lifecycle {
    use super::*;
    use std::ffi::{OsStr, c_void};
    use std::io;
    use std::os::windows::{ffi::OsStrExt, process::CommandExt};
    use std::process::Stdio;

    const EVENT_MODIFY_STATE: u32 = 0x0002;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const ERROR_FILE_NOT_FOUND: u32 = 2;
    const ERROR_PATH_NOT_FOUND: u32 = 3;
    const ERROR_SERVICE_DOES_NOT_EXIST: u32 = 1060;
    const HKEY_CURRENT_USER: isize = -2_147_483_647;
    const KEY_QUERY_VALUE: u32 = 0x0001;
    const REG_SZ: u32 = 1;
    const SC_MANAGER_CONNECT: u32 = 0x0001;
    const SERVICE_QUERY_STATUS: u32 = 0x0004;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegOpenKeyExW(
            key: *mut c_void,
            subkey: *const u16,
            options: u32,
            access: u32,
            result: *mut *mut c_void,
        ) -> i32;
        fn RegQueryValueExW(
            key: *mut c_void,
            value_name: *const u16,
            reserved: *mut u32,
            value_type: *mut u32,
            data: *mut u8,
            data_size: *mut u32,
        ) -> i32;
        fn RegCloseKey(key: *mut c_void) -> i32;
        fn OpenSCManagerW(
            machine_name: *const u16,
            database_name: *const u16,
            access: u32,
        ) -> *mut c_void;
        fn OpenServiceW(manager: *mut c_void, service_name: *const u16, access: u32)
        -> *mut c_void;
        fn QueryServiceStatus(service: *mut c_void, status: *mut ServiceStatus) -> i32;
        fn CloseServiceHandle(handle: *mut c_void) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenEventW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
        fn SetEvent(event: *mut c_void) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum DaemonAction {
        Start,
        Stop,
        Restart,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum DaemonActionResult {
        Started,
        AlreadyRunning,
        Stopped,
        Restarted,
    }

    pub struct DaemonActionTask(Receiver<Result<DaemonActionResult, String>>);

    impl DaemonActionTask {
        pub fn start(action: DaemonAction) -> Self {
            let (sender, receiver) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let _ = sender.send(run_action(action));
            });
            Self(receiver)
        }

        pub fn poll(&self) -> Option<Result<DaemonActionResult, String>> {
            match self.0.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    "Windows daemon lifecycle worker stopped without a result".to_owned(),
                )),
            }
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct InstallationStatus {
        pub startup: String,
        pub broker: String,
        pub driver: String,
    }

    pub struct InstallationQuery(Receiver<InstallationStatus>);

    impl InstallationQuery {
        pub fn start() -> Self {
            let (sender, receiver) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let status = InstallationStatus {
                    startup: startup_status(),
                    broker: component_status("WiiLandOutput"),
                    driver: component_status("WiiLandVhid"),
                };
                let _ = sender.send(status);
            });
            Self(receiver)
        }

        pub fn poll(&self) -> Option<InstallationStatus> {
            match self.0.try_recv() {
                Ok(status) => Some(status),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(InstallationStatus {
                    startup: "Unavailable".to_owned(),
                    broker: "Unavailable".to_owned(),
                    driver: "Unavailable".to_owned(),
                }),
            }
        }
    }

    pub fn trusted_daemon_path() -> Result<PathBuf, String> {
        installed_daemon_path()
    }

    fn installed_daemon_path() -> Result<PathBuf, String> {
        let program_files = std::env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .ok_or_else(|| "Windows did not provide the ProgramFiles location".to_owned())?;
        if !program_files.is_absolute() {
            return Err("ProgramFiles is not an absolute path".to_owned());
        }
        let install = program_files.join("WiiLand");
        let install_metadata = std::fs::symlink_metadata(&install)
            .map_err(|error| format!("WiiLand install directory is unavailable: {error}"))?;
        if !install_metadata.is_dir() || install_metadata.file_type().is_symlink() {
            return Err(
                "WiiLand install directory is not a regular Program Files directory".into(),
            );
        }
        let canonical_program_files = program_files
            .canonicalize()
            .map_err(|error| format!("ProgramFiles cannot be resolved: {error}"))?;
        let canonical_install = install
            .canonicalize()
            .map_err(|error| format!("WiiLand install directory cannot be resolved: {error}"))?;
        if canonical_install.parent() != Some(canonical_program_files.as_path())
            || !canonical_install
                .file_name()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("WiiLand"))
        {
            return Err("WiiLand is not installed directly under ProgramFiles".into());
        }
        let executable = install.join("wiilandd.exe");
        let executable_metadata = std::fs::symlink_metadata(&executable)
            .map_err(|error| format!("Installed wiilandd.exe is unavailable: {error}"))?;
        if !executable_metadata.is_file() || executable_metadata.file_type().is_symlink() {
            return Err("Installed wiilandd.exe is not a regular file".into());
        }
        let canonical_executable = executable
            .canonicalize()
            .map_err(|error| format!("Installed wiilandd.exe cannot be resolved: {error}"))?;
        if canonical_executable.parent() != Some(canonical_install.as_path()) {
            return Err(
                "Installed wiilandd.exe resolves outside the WiiLand install directory".into(),
            );
        }
        Ok(executable)
    }

    fn startup_status() -> String {
        match installed_daemon_path() {
            Ok(executable) => match run_value_matches(&executable) {
                Ok(true) => "Configured · HKCU Run WiiLandDaemon".to_owned(),
                Ok(false) => "Not configured · HKCU Run WiiLandDaemon".to_owned(),
                Err(error) => format!("Unavailable · {error}"),
            },
            Err(error) => format!("Unavailable · {error}"),
        }
    }

    fn run_value_matches(executable: &std::path::Path) -> io::Result<bool> {
        let subkey = wide_null(OsStr::new(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
        ));
        let value_name = wide_null(OsStr::new("WiiLandDaemon"));
        let mut key = std::ptr::null_mut();
        let opened = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER as *mut c_void,
                subkey.as_ptr(),
                0,
                KEY_QUERY_VALUE,
                &mut key,
            )
        };
        if opened == ERROR_FILE_NOT_FOUND as i32 || opened == ERROR_PATH_NOT_FOUND as i32 {
            return Ok(false);
        }
        if opened != 0 {
            return Err(io::Error::from_raw_os_error(opened));
        }
        let _key = RegistryKey(key);

        let mut value_type = 0;
        let mut data_size = 0;
        let queried = unsafe {
            RegQueryValueExW(
                key,
                value_name.as_ptr(),
                std::ptr::null_mut(),
                &mut value_type,
                std::ptr::null_mut(),
                &mut data_size,
            )
        };
        if queried == ERROR_FILE_NOT_FOUND as i32 {
            return Ok(false);
        }
        if queried != 0 {
            return Err(io::Error::from_raw_os_error(queried));
        }
        if value_type != REG_SZ || data_size % 2 != 0 || data_size > 64 * 1024 {
            return Ok(false);
        }
        let mut data = vec![0u16; data_size as usize / 2];
        let queried = unsafe {
            RegQueryValueExW(
                key,
                value_name.as_ptr(),
                std::ptr::null_mut(),
                &mut value_type,
                data.as_mut_ptr().cast(),
                &mut data_size,
            )
        };
        if queried != 0 {
            return Err(io::Error::from_raw_os_error(queried));
        }
        if value_type != REG_SZ {
            return Ok(false);
        }
        let length = data
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(data.len());
        data.truncate(length);

        let mut expected = Vec::with_capacity(executable.as_os_str().len() + 2);
        expected.push(u16::from(b'"'));
        expected.extend(executable.as_os_str().encode_wide());
        expected.push(u16::from(b'"'));
        Ok(data == expected)
    }

    fn run_action(action: DaemonAction) -> Result<DaemonActionResult, String> {
        match action {
            DaemonAction::Start => start_daemon(),
            DaemonAction::Stop => {
                stop_daemon()?;
                Ok(DaemonActionResult::Stopped)
            }
            DaemonAction::Restart => {
                if daemon_is_reachable().map_err(|error| {
                    format!("Cannot verify the current daemon before restart: {error}")
                })? {
                    stop_daemon()?;
                }
                start_daemon()?;
                Ok(DaemonActionResult::Restarted)
            }
        }
    }

    fn start_daemon() -> Result<DaemonActionResult, String> {
        if daemon_is_reachable()
            .map_err(|error| format!("Cannot verify the current daemon: {error}"))?
        {
            return Ok(DaemonActionResult::AlreadyRunning);
        }
        wait_for_stop_event_absent()?;
        let executable = trusted_daemon_path()?;
        std::process::Command::new(executable)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|error| format!("Could not start installed wiilandd.exe: {error}"))?;
        wait_for_daemon_state(true)?;
        Ok(DaemonActionResult::Started)
    }

    fn stop_daemon() -> Result<(), String> {
        let event_name = stop_event_name()?;
        let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr()) };
        if event.is_null() {
            let error = unsafe { GetLastError() };
            if error == ERROR_FILE_NOT_FOUND {
                return Err(
                    "The daemon stop event is absent; WiiLand daemon is not running".into(),
                );
            }
            return Err(format!(
                "Cannot open the per-logon daemon stop event: {}",
                io::Error::from_raw_os_error(error as i32)
            ));
        }
        let _event = EventHandle(event);
        if unsafe { SetEvent(event) } == 0 {
            return Err(format!(
                "Cannot request graceful daemon shutdown: {}",
                io::Error::last_os_error()
            ));
        }
        drop(_event);
        wait_for_daemon_state(false)
    }

    fn stop_event_name() -> Result<Vec<u16>, String> {
        let pipe_path = wiiland_ipc::Client::default_socket_path().map_err(|error| {
            format!("Cannot resolve the current Windows logon endpoint: {error}")
        })?;
        let pipe_path = pipe_path
            .to_str()
            .ok_or_else(|| "Windows returned a non-Unicode daemon endpoint".to_owned())?;
        let logon_sid = pipe_path
            .strip_prefix(r"\\.\pipe\WiiLand.")
            .and_then(|name| name.strip_suffix(".daemon"))
            .filter(|sid| is_logon_sid(sid))
            .ok_or_else(|| "Windows returned an unexpected logon-session endpoint".to_owned())?;
        let event_name = format!(r"Local\WiiLandDaemonStop.{logon_sid}");
        Ok(wide_null(OsStr::new(&event_name)))
    }

    fn wait_for_stop_event_absent() -> Result<(), String> {
        let event_name = stop_event_name()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr()) };
            if event.is_null() {
                let error = unsafe { GetLastError() };
                if error == ERROR_FILE_NOT_FOUND {
                    return Ok(());
                }
                return Err(format!(
                    "Cannot check the per-logon daemon stop event: {}",
                    io::Error::from_raw_os_error(error as i32)
                ));
            }
            drop(EventHandle(event));
            if Instant::now() >= deadline {
                return Err("The per-logon daemon control event remains present; refusing a duplicate launch".to_owned());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    pub(super) fn stop_event_present() -> Result<bool, String> {
        let event_name = stop_event_name()?;
        let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr()) };
        if event.is_null() {
            let error = unsafe { GetLastError() };
            if error == ERROR_FILE_NOT_FOUND || error == ERROR_PATH_NOT_FOUND {
                return Ok(false);
            }
            return Err(format!(
                "Cannot inspect the per-logon daemon stop event: {}",
                io::Error::from_raw_os_error(error as i32)
            ));
        }
        drop(EventHandle(event));
        Ok(true)
    }

    fn is_logon_sid(value: &str) -> bool {
        let Some(parts) = value.strip_prefix("S-1-5-5-") else {
            return false;
        };
        let mut parts = parts.split('-');
        parts.next().is_some_and(decimal_component)
            && parts.next().is_some_and(decimal_component)
            && parts.next().is_none()
    }

    fn decimal_component(value: &str) -> bool {
        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
    }

    fn daemon_is_reachable() -> Result<bool, wiiland_ipc::ClientError> {
        let mut client = match wiiland_ipc::Client::connect_default() {
            Ok(client) => client,
            Err(error) if endpoint_absent(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        client.set_read_timeout(Some(Duration::from_millis(500)))?;
        client.set_write_timeout(Some(Duration::from_millis(500)))?;
        client.status().map(|_| true)
    }

    pub(super) fn endpoint_absent(error: &wiiland_ipc::ClientError) -> bool {
        matches!(
            error,
            wiiland_ipc::ClientError::Io(error)
                if error.kind() == io::ErrorKind::NotFound
                    || matches!(error.raw_os_error(), Some(2 | 3))
        )
    }

    fn retryable_during_shutdown(error: &wiiland_ipc::ClientError) -> bool {
        match error {
            wiiland_ipc::ClientError::Io(error) => {
                matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::BrokenPipe
                ) || matches!(error.raw_os_error(), Some(2 | 3 | 109 | 121 | 231 | 233))
            }
            wiiland_ipc::ClientError::PrematureEof => true,
            _ => false,
        }
    }

    fn wait_for_daemon_state(expected_running: bool) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match daemon_is_reachable() {
                Ok(running) if running == expected_running => return Ok(()),
                Ok(_) => {}
                Err(error) if retryable_during_shutdown(&error) => {}
                Err(error) => {
                    return Err(format!("Cannot confirm daemon lifecycle state: {error}"));
                }
            }
            if Instant::now() >= deadline {
                return Err(if expected_running {
                    "wiilandd.exe was launched but its authenticated IPC endpoint did not become ready"
                        .to_owned()
                } else {
                    "WiiLand daemon did not finish graceful shutdown before the timeout".to_owned()
                });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn component_status(name: &str) -> String {
        let manager =
            unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) };
        if manager.is_null() {
            return format!("Unavailable · {}", io::Error::last_os_error());
        }
        let _manager = ServiceHandle(manager);
        let service_name = wide_null(OsStr::new(name));
        let service = unsafe { OpenServiceW(manager, service_name.as_ptr(), SERVICE_QUERY_STATUS) };
        if service.is_null() {
            let error = unsafe { GetLastError() };
            return if error == ERROR_SERVICE_DOES_NOT_EXIST {
                "Not installed".to_owned()
            } else {
                format!(
                    "Unavailable · {}",
                    io::Error::from_raw_os_error(error as i32)
                )
            };
        }
        let _service = ServiceHandle(service);
        let mut status = ServiceStatus::default();
        if unsafe { QueryServiceStatus(service, &mut status) } == 0 {
            return format!(
                "Installed · status unavailable ({})",
                io::Error::last_os_error()
            );
        }
        let state = match status.current_state {
            1 => "Stopped",
            2 => "Starting",
            3 => "Stopping",
            4 => "Running",
            5 => "Resuming",
            6 => "Pausing",
            7 => "Paused",
            _ => "Unknown",
        };
        format!("Installed · {state}")
    }

    fn wide_null(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    struct RegistryKey(*mut c_void);

    impl Drop for RegistryKey {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }

    struct ServiceHandle(*mut c_void);

    impl Drop for ServiceHandle {
        fn drop(&mut self) {
            unsafe {
                CloseServiceHandle(self.0);
            }
        }
    }

    struct EventHandle(*mut c_void);

    impl Drop for EventHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    #[repr(C)]
    #[derive(Default)]
    struct ServiceStatus {
        service_type: u32,
        current_state: u32,
        controls_accepted: u32,
        win32_exit_code: u32,
        service_specific_exit_code: u32,
        check_point: u32,
        wait_hint: u32,
    }
}

#[cfg(windows)]
pub use windows_lifecycle::{
    DaemonAction, DaemonActionResult, DaemonActionTask, InstallationQuery, InstallationStatus,
    trusted_daemon_path,
};

#[cfg(all(test, not(windows)))]
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
