//! Native Windows daemon reactor. The reactor owns HID sessions and dispatches
//! one shared named-pipe completion event without Unix descriptors or syscalls.
#![cfg(windows)]
#![allow(unsafe_code)]

use crate::bridge::{BridgeAction, WindowsBridgeDevice};
use crate::cli::{Action, Cli, IpcMode};
use crate::ipc::{IpcServer, PollSource};
use crate::windows_output_worker::OutputWorkerStatus;
use std::cell::Cell;
use std::ffi::{OsStr, c_void};
use std::io::{self, Write};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use wiiland_core::{Config, TraceConfig};
use wiiland_hid::{
    Axis3 as Abs, Button, ButtonEvent as HidButtonEvent, ButtonState, Event, EventKind, Monitor,
    MonitorMode, MonitorPoll,
};
use wiiland_ipc::{
    Axis3, ButtonEvent, DeviceInfo, InputPayload, Notification, RemovalReason, Status, Timestamp,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE,
    SetLastError,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, IsValidSid, SECURITY_ATTRIBUTES, TOKEN_GROUPS, TOKEN_QUERY, TokenLogonSid,
};
use windows_sys::Win32::System::Threading::{
    CreateEventExW, GetCurrentProcess, OpenProcessToken, SetEvent, WaitForMultipleObjects,
};

const MAX_DEVICES: usize = 32;
const MAX_OUTPUT_WORKERS: usize = 32;
const MAX_WAIT_HANDLES: usize = MAX_DEVICES + 2;
const MAX_EVENTS_PER_DRAIN: usize = 256;
const MONITOR_SCAN_BUDGET: usize = 64;
const POINTER_TICK: Duration = Duration::from_millis(16);
const OUTPUT_REFRESH_TICK: Duration = Duration::from_millis(250);
const RECONCILE_TICK: Duration = Duration::from_secs(1);
const EVENT_NAME_PREFIX: &str = r"Local\WiiLandDaemonStop.";
const LOGON_ID_GROUP_ATTRIBUTES: u32 = 0xC000_0000;
const SDDL_REVISION_1: u32 = 1;
const CREATE_EVENT_MANUAL_RESET: u32 = 0x0000_0001;
const EVENT_MODIFY_STATE: u32 = 0x0000_0002;
const SYNCHRONIZE: u32 = 0x0010_0000;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x0000_0102;
const WAIT_FAILED: u32 = 0xffff_ffff;
const CTRL_C_EVENT: u32 = 0;
const CTRL_BREAK_EVENT: u32 = 1;
const CTRL_CLOSE_EVENT: u32 = 2;
const CTRL_LOGOFF_EVENT: u32 = 5;
const CTRL_SHUTDOWN_EVENT: u32 = 6;

static CONSOLE_STOP_EVENT: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

type ConsoleHandler = Option<unsafe extern "system" fn(u32) -> i32>;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(handler: ConsoleHandler, add: i32) -> i32;
}

#[derive(Debug)]
struct RuntimeFailure {
    code: i32,
    message: String,
}

impl RuntimeFailure {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn io(operation: &str, error: io::Error) -> Self {
        let code = -error.raw_os_error().unwrap_or(libc::EIO);
        Self::new(code, format!("wiilandd: {operation}: {error}"))
    }
}
const MAX_LOG_RECORDS: usize = 256;
const MAX_LOG_RECORD_BYTES: usize = 16 * 1024;

#[derive(Clone)]
struct LogSender {
    sender: SyncSender<String>,
    dropped: Arc<AtomicU64>,
}

impl LogSender {
    fn send(&self, line: &str) {
        if line.len() > MAX_LOG_RECORD_BYTES || self.sender.try_send(line.to_owned()).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

struct WindowsLogWriter {
    sender: Option<LogSender>,
    finished: Receiver<()>,
    worker: Option<JoinHandle<()>>,
}

impl WindowsLogWriter {
    fn stdout() -> io::Result<Self> {
        Self::spawn("wiiland-windows-trace", || Box::new(io::stdout()))
    }

    fn stderr() -> io::Result<Self> {
        Self::spawn("wiiland-windows-diagnostics", || Box::new(io::stderr()))
    }

    fn spawn(
        name: &'static str,
        output: impl FnOnce() -> Box<dyn Write + Send> + Send + 'static,
    ) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<String>(MAX_LOG_RECORDS);
        let (done, finished) = mpsc::sync_channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&dropped);
        let worker = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let mut writer = output();
                let mut reported = 0;
                for line in receiver {
                    let current = counter.load(Ordering::Relaxed);
                    if current != reported {
                        if writeln!(
                            writer,
                            "wiilandd: diagnostics: dropped {} records",
                            current - reported
                        )
                        .and_then(|_| writer.flush())
                        .is_err()
                        {
                            break;
                        }
                        reported = current;
                    }
                    if writeln!(writer, "{line}")
                        .and_then(|_| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
                let _ = done.send(());
            })?;
        Ok(Self {
            sender: Some(LogSender { sender, dropped }),
            finished,
            worker: Some(worker),
        })
    }

    fn sender(&self) -> LogSender {
        self.sender.as_ref().expect("writer is open").clone()
    }
}

impl Drop for WindowsLogWriter {
    fn drop(&mut self) {
        self.sender.take();
        if self
            .finished
            .recv_timeout(Duration::from_millis(100))
            .is_ok()
            && let Some(worker) = self.worker.take()
        {
            let _ = worker.join();
        }
    }
}

fn exit_code(code: i32) -> i32 {
    code.unsigned_abs().min(255) as i32
}

fn next_sequence(sequence: &Cell<u64>) -> u64 {
    let mut next = sequence.get().wrapping_add(1);
    if next == 0 {
        next = 1;
    }
    sequence.set(next);
    next
}

fn axis(value: Abs) -> Axis3 {
    Axis3 {
        x: value.x,
        y: value.y,
        z: value.z,
    }
}

fn button_code(button: Button) -> u32 {
    button.code()
}

fn button_state(state: ButtonState) -> u32 {
    state.value()
}

fn button(value: HidButtonEvent) -> ButtonEvent {
    ButtonEvent {
        code: button_code(value.button),
        state: button_state(value.state),
    }
}

fn event_type_code(kind: EventKind) -> u32 {
    kind.event_type().code()
}
fn input_payload(kind: EventKind) -> InputPayload {
    let raw = event_type_code(kind);
    match kind {
        EventKind::Key(value) => InputPayload::Key(button(value)),
        EventKind::Accel(value) => InputPayload::Accel(axis(value)),
        EventKind::Ir(values) => InputPayload::Ir(values.map(axis)),
        EventKind::BalanceBoard(values) => InputPayload::BalanceBoard(values.map(axis)),
        EventKind::MotionPlus(value) => InputPayload::MotionPlus(axis(value)),
        EventKind::ProControllerKey(value) => InputPayload::ProControllerKey(button(value)),
        EventKind::ProControllerMove(values) => InputPayload::ProControllerMove(values.map(axis)),
        EventKind::Watch => InputPayload::Watch,
        EventKind::ClassicControllerKey(value) => InputPayload::ClassicControllerKey(button(value)),
        EventKind::ClassicControllerMove(values) => {
            InputPayload::ClassicControllerMove(values.map(axis))
        }
        EventKind::NunchukKey(value) => InputPayload::NunchukKey(button(value)),
        EventKind::NunchukMove(values) => InputPayload::NunchukMove(values.map(axis)),
        EventKind::DrumsKey(value) => InputPayload::DrumsKey(button(value)),
        EventKind::DrumsMove(values) => InputPayload::DrumsMove(values.map(axis)),
        EventKind::GuitarKey(value) => InputPayload::GuitarKey(button(value)),
        EventKind::GuitarMove(values) => InputPayload::GuitarMove(values.map(axis)),
        EventKind::Gone => InputPayload::Gone,
        EventKind::Unknown(value) => InputPayload::Unknown(value),
        _ => InputPayload::Unknown(raw),
    }
}

fn timestamp(event: &Event) -> Timestamp {
    Timestamp {
        seconds: event.time.seconds,
        micros: event.time.microseconds,
    }
}

struct StopEvent {
    handle: HANDLE,
    registered_console_handler: bool,
}

impl StopEvent {
    fn create() -> io::Result<Self> {
        let logon_sid = current_logon_sid_string()?;
        let event_name = format!("{EVENT_NAME_PREFIX}{logon_sid}");
        let wide_name = nul_terminated_wide(OsStr::new(&event_name));
        let sddl = format!("D:P(D;;WDWO;;;OW)(A;;GA;;;{logon_sid})(A;;GA;;;SY)");
        let wide_sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide_sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if descriptor.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows returned a null stop-event security descriptor",
            ));
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.cast(),
            bInheritHandle: 0,
        };
        unsafe { SetLastError(0) };
        let handle = unsafe {
            CreateEventExW(
                &attributes,
                wide_name.as_ptr(),
                CREATE_EVENT_MANUAL_RESET,
                EVENT_MODIFY_STATE | SYNCHRONIZE,
            )
        };
        let creation_error = unsafe { GetLastError() };
        unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor.cast()) };
        if handle.is_null() {
            return Err(io::Error::from_raw_os_error(creation_error as i32));
        }
        if creation_error == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(handle) };
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a WiiLand daemon already owns the per-logon stop event",
            ));
        }

        CONSOLE_STOP_EVENT.store(handle, Ordering::Release);
        let registered_console_handler =
            unsafe { SetConsoleCtrlHandler(Some(console_handler), 1) } != 0;
        if !registered_console_handler {
            let error = unsafe { GetLastError() };
            CONSOLE_STOP_EVENT.store(ptr::null_mut(), Ordering::Release);
            unsafe { CloseHandle(handle) };
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        Ok(Self {
            handle,
            registered_console_handler,
        })
    }

    fn raw(&self) -> HANDLE {
        self.handle
    }
}

impl Drop for StopEvent {
    fn drop(&mut self) {
        if self.registered_console_handler {
            unsafe { SetConsoleCtrlHandler(Some(console_handler), 0) };
        }
        CONSOLE_STOP_EVENT.store(ptr::null_mut(), Ordering::Release);
        if !self.handle.is_null() {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

unsafe extern "system" fn console_handler(event: u32) -> i32 {
    if !matches!(
        event,
        CTRL_C_EVENT
            | CTRL_BREAK_EVENT
            | CTRL_CLOSE_EVENT
            | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT
    ) {
        return 0;
    }
    let stop_event = CONSOLE_STOP_EVENT.load(Ordering::Acquire);
    if stop_event.is_null() {
        return 0;
    }
    if unsafe { SetEvent(stop_event) } != 0 {
        1
    } else {
        0
    }
}

fn current_logon_sid_string() -> io::Result<String> {
    let mut raw_token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = raw_token;
    let mut required = 0;
    let queried =
        unsafe { GetTokenInformation(token, TokenLogonSid, ptr::null_mut(), 0, &mut required) };
    let query_error = unsafe { GetLastError() };
    if queried != 0 {
        unsafe { CloseHandle(token) };
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned token data for a zero-sized query",
        ));
    }
    if query_error != ERROR_INSUFFICIENT_BUFFER || required == 0 {
        unsafe { CloseHandle(token) };
        return Err(io::Error::from_raw_os_error(query_error as i32));
    }
    let words = (required as usize).div_ceil(size_of::<usize>());
    let mut storage = vec![0usize; words];
    let Some(bytes) = words
        .checked_mul(size_of::<usize>())
        .and_then(|n| u32::try_from(n).ok())
    else {
        unsafe { CloseHandle(token) };
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "token SID data is too large",
        ));
    };
    let mut returned = required;
    let got_information = unsafe {
        GetTokenInformation(
            token,
            TokenLogonSid,
            storage.as_mut_ptr().cast(),
            bytes,
            &mut returned,
        )
    };
    let information_error = if got_information == 0 {
        unsafe { GetLastError() }
    } else {
        0
    };
    unsafe { CloseHandle(token) };
    if got_information == 0 {
        return Err(io::Error::from_raw_os_error(information_error as i32));
    }
    let sid = unsafe {
        let groups = &*storage.as_ptr().cast::<TOKEN_GROUPS>();
        if groups.GroupCount != 1
            || groups.Groups[0].Attributes & LOGON_ID_GROUP_ATTRIBUTES != LOGON_ID_GROUP_ATTRIBUTES
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process token did not return exactly one logon SID",
            ));
        }
        groups.Groups[0].Sid
    };
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process token returned an invalid logon SID",
        ));
    }
    let mut sid_name = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut sid_name) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if sid_name.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a null logon SID string",
        ));
    }
    let mut length = 0usize;
    unsafe {
        while *sid_name.add(length) != 0 {
            length += 1;
        }
    }
    let value = String::from_utf16(unsafe { std::slice::from_raw_parts(sid_name, length) })
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "logon SID string is not UTF-16"));
    unsafe { windows_sys::Win32::Foundation::LocalFree(sid_name.cast::<c_void>()) };
    let value = value?;
    if !value.starts_with("S-1-5-5-") || value.len() > 128 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process token returned an invalid logon SID",
        ));
    }
    Ok(value)
}

fn nul_terminated_wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[derive(Clone, Copy)]
enum WaitTarget {
    Stop,
    Ipc(u64),
    Device(usize),
}

struct WindowsRuntime {
    config: Config,
    dry_run: bool,
    trace: TraceConfig,
    selected_path: Option<PathBuf>,
    slots: Vec<Option<WindowsBridgeDevice>>,
    retired_output_workers: Vec<JoinHandle<()>>,
    monitor: Option<Monitor>,
    monitor_pending: bool,
    next_reconcile: Instant,
    next_pointer: Option<Instant>,
    next_output_refresh: Instant,
    stop_event: StopEvent,
    ipc: Option<IpcServer>,
    ipc_sources: Vec<PollSource>,
    wait_handles: Vec<HANDLE>,
    wait_targets: Vec<WaitTarget>,
    next_wait_source: usize,
    input_notifications: [Option<Notification>; MAX_EVENTS_PER_DRAIN],
    notification_sequence: Rc<Cell<u64>>,
    trace_sequence: Rc<Cell<u64>>,
    diagnostics: LogSender,
    trace_sender: LogSender,
    trace_dropped: LogSender,
    _diagnostic_writer: WindowsLogWriter,
    _trace_writer: WindowsLogWriter,
    metrics: wiiland_ipc::Diagnostics,
}

impl WindowsRuntime {
    fn new(cli: &Cli) -> Result<Self, RuntimeFailure> {
        let mut config = cli.config.clone();
        config.validate().map_err(|error| {
            RuntimeFailure::new(
                -libc::EINVAL,
                format!("wiilandd: invalid configuration: {error}"),
            )
        })?;
        config.backend = config
            .backend
            .resolve_for_current_platform()
            .map_err(|message| {
                RuntimeFailure::new(-libc::EINVAL, format!("wiilandd: {message}"))
            })?;
        let stop_event = StopEvent::create()
            .map_err(|error| RuntimeFailure::io("cannot create per-logon stop event", error))?;
        let selected_path = if let Some(selector) = cli.device.as_deref() {
            Some(crate::cli::resolve_device_arg(selector).ok_or_else(|| {
                RuntimeFailure::new(
                    -libc::ENODEV,
                    "wiilandd: cannot resolve device; run --list and pass --device <number|HID path>",
                )
            })?)
        } else {
            None
        };

        let ipc_path = match &cli.ipc {
            IpcMode::Disabled => None,
            IpcMode::Path(path) => Some(path.clone()),
            IpcMode::Auto => Some(wiiland_ipc::default_socket_path().map_err(|error| {
                RuntimeFailure::new(
                    -libc::EINVAL,
                    format!("wiilandd: cannot determine default Windows IPC endpoint: {error}"),
                )
            })?),
        };
        let ipc = ipc_path
            .as_ref()
            .map(|path| {
                IpcServer::bind(path).map_err(|error| {
                    RuntimeFailure::io(
                        &format!("cannot bind Windows named pipe {}", path.display()),
                        error,
                    )
                })
            })
            .transpose()?;
        let monitor = if selected_path.is_none() {
            Some(Monitor::new(MonitorMode::Enumerate).map_err(|error| {
                RuntimeFailure::io("cannot create HID discovery monitor", error)
            })?)
        } else {
            None
        };
        let diagnostic_writer = WindowsLogWriter::stderr()
            .map_err(|error| RuntimeFailure::io("cannot start diagnostic writer", error))?;
        let trace_writer = WindowsLogWriter::stdout()
            .map_err(|error| RuntimeFailure::io("cannot start trace writer", error))?;
        let diagnostics = diagnostic_writer.sender();
        let trace_sender = trace_writer.sender();
        let trace_dropped = trace_sender.clone();
        let now = Instant::now();
        let monitor_pending = monitor.is_some();
        let runtime = Self {
            config,
            dry_run: cli.dry_run,
            trace: cli.trace,
            selected_path,
            slots: std::iter::repeat_with(|| None).take(MAX_DEVICES).collect(),
            retired_output_workers: Vec::with_capacity(MAX_OUTPUT_WORKERS),
            monitor,
            monitor_pending,
            next_reconcile: now + RECONCILE_TICK,
            next_pointer: now.checked_add(POINTER_TICK),
            next_output_refresh: now + OUTPUT_REFRESH_TICK,
            stop_event,
            ipc,
            ipc_sources: Vec::with_capacity(1),
            wait_handles: Vec::with_capacity(MAX_WAIT_HANDLES),
            wait_targets: Vec::with_capacity(MAX_WAIT_HANDLES),
            next_wait_source: 0,
            input_notifications: std::array::from_fn(|_| None),
            notification_sequence: Rc::new(Cell::new(0)),
            trace_sequence: Rc::new(Cell::new(0)),
            diagnostics,
            trace_sender,
            trace_dropped,
            _diagnostic_writer: diagnostic_writer,
            _trace_writer: trace_writer,
            metrics: wiiland_ipc::Diagnostics::default(),
        };
        Ok(runtime)
    }

    fn emit(&self, line: &str) {
        self.diagnostics.send(line);
    }

    fn find(&self, path: &Path) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|device| device.path() == path))
    }

    fn output_worker_permits_in_use(&self) -> usize {
        self.retired_output_workers.len()
            + self
                .slots
                .iter()
                .filter(|slot| {
                    slot.as_ref()
                        .is_some_and(WindowsBridgeDevice::has_output_worker)
                })
                .count()
    }

    fn reap_finished_output_workers(&mut self) {
        let mut index = 0;
        while index < self.retired_output_workers.len() {
            if self.retired_output_workers[index].is_finished() {
                let worker = self.retired_output_workers.swap_remove(index);
                let _ = worker.join();
            } else {
                index += 1;
            }
        }
    }

    fn open_device(&mut self, path: &Path) -> Result<bool, i32> {
        if self.find(path).is_some() {
            return Ok(false);
        }
        let Some(slot) = self.slots.iter().position(Option::is_none) else {
            self.emit(&format!(
                "wiilandd: error: device limit reached ({}): {}",
                MAX_DEVICES,
                path.display()
            ));
            return Err(-libc::ENOSPC);
        };
        self.reap_finished_output_workers();
        let output_worker_permit_available =
            self.output_worker_permits_in_use() < MAX_OUTPUT_WORKERS;
        match WindowsBridgeDevice::new_with_output_worker_permit(
            path,
            &self.config,
            !self.dry_run,
            output_worker_permit_available,
        ) {
            Ok(mut device) => {
                if self.trace.enabled {
                    let sender = self.trace_sender.clone();
                    device.set_trace_sink_with_sequence(
                        self.trace.filter,
                        Rc::clone(&self.trace_sequence),
                        Box::new(move |line: &str| sender.send(line)),
                    );
                }
                let info = device.info();
                self.slots[slot] = Some(device);
                self.publish(Notification::DeviceAdded {
                    sequence: next_sequence(&self.notification_sequence),
                    device: info,
                });
                self.emit(&format!("wiilandd: add: {}", path.display()));
                self.service_capture_leases();
                Ok(true)
            }
            Err(code) => {
                self.emit(&format!("wiilandd: error: add {}: {code}", path.display()));
                Err(code)
            }
        }
    }

    fn publish(&mut self, notification: Notification) {
        if let Some(server) = self.ipc.as_mut() {
            server.publish(notification);
        }
    }

    fn publish_removed(&mut self, path: &Path, reason: RemovalReason) {
        self.publish(Notification::DeviceRemoved {
            sequence: next_sequence(&self.notification_sequence),
            syspath: path.to_string_lossy().into_owned(),
            reason,
        });
    }

    fn remove_device(
        &mut self,
        slot: usize,
        reason: RemovalReason,
        operation: Option<&str>,
        code: Option<i32>,
    ) {
        let Some(mut device) = self.slots[slot].take() else {
            return;
        };
        let path = device.path().to_path_buf();
        if let Some(worker) = device.request_output_close() {
            self.retired_output_workers.push(worker);
        }
        self.publish_removed(&path, reason);
        if let (Some(operation), Some(code)) = (operation, code) {
            self.emit(&format!(
                "wiilandd: error: {operation} {}: {code}",
                path.display()
            ));
        } else {
            self.emit(&format!("wiilandd: remove: {}", path.display()));
        }
    }

    fn has_input_subscribers(&self) -> bool {
        self.ipc
            .as_ref()
            .is_some_and(IpcServer::has_input_subscribers)
    }

    fn drain_device(&mut self, slot: usize) {
        let Some(path) = self.slots[slot]
            .as_ref()
            .map(|device| device.path().to_path_buf())
        else {
            return;
        };
        let outcome = if self.has_input_subscribers() {
            let sequence = Rc::clone(&self.notification_sequence);
            let syspath = path.to_string_lossy().into_owned();
            let (result, count) = {
                let notifications = &mut self.input_notifications;
                let mut count = 0;
                let result = self.slots[slot].as_mut().map(|device| {
                    device.drain(&mut |event| {
                        if count < notifications.len() {
                            notifications[count] = Some(Notification::Input {
                                sequence: next_sequence(&sequence),
                                syspath: syspath.clone(),
                                timestamp: timestamp(event),
                                payload: input_payload(event.kind),
                            });
                            count += 1;
                        }
                    })
                });
                (result, count)
            };
            for index in 0..count {
                if let Some(notification) = self.input_notifications[index].take() {
                    self.publish(notification);
                }
            }
            result
        } else {
            self.slots[slot]
                .as_mut()
                .map(|device| device.drain(&mut |_| {}))
        };
        match outcome {
            Some(Ok(BridgeAction::Continue)) | None => {}
            Some(Ok(BridgeAction::Gone)) => {
                self.remove_device(slot, RemovalReason::Gone, None, None);
            }
            Some(Err(code)) => {
                self.remove_device(slot, RemovalReason::DrainError, Some("drain"), Some(code));
            }
        }
    }

    fn retry_pending_interfaces(&mut self) {
        for slot in 0..MAX_DEVICES {
            let result = self.slots[slot]
                .as_mut()
                .map(WindowsBridgeDevice::retry_open);
            if let Some(Err(code)) = result {
                self.remove_device(
                    slot,
                    RemovalReason::DrainError,
                    Some("interface retry"),
                    Some(code),
                );
            }
        }
    }
    fn service_discovery(&mut self, now: Instant) {
        if self.selected_path.is_some() {
            if now >= self.next_reconcile {
                self.retry_pending_interfaces();
                self.next_reconcile = now + RECONCILE_TICK;
            }
            return;
        }
        if !self.monitor_pending {
            if now < self.next_reconcile {
                return;
            }
            self.retry_pending_interfaces();
            self.monitor_pending = true;
        }
        if self.monitor.is_none() {
            if now < self.next_reconcile {
                return;
            }
            match Monitor::new(MonitorMode::Enumerate) {
                Ok(monitor) => {
                    self.monitor = Some(monitor);
                }
                Err(error) => {
                    self.emit(&format!("wiilandd: error: HID discovery: {error}"));
                    self.monitor_pending = false;
                    self.next_reconcile = now + RECONCILE_TICK;
                    return;
                }
            }
        }
        let result = self
            .monitor
            .as_mut()
            .expect("checked monitor")
            .poll_bounded(MONITOR_SCAN_BUDGET);
        match result {
            Ok(MonitorPoll::Path(path)) => {
                if self.find(&path).is_none() && self.slots.iter().any(Option::is_none) {
                    let _ = self.open_device(&path);
                }
                self.monitor_pending = true;
            }
            Ok(MonitorPoll::Pending) => {
                self.monitor_pending = true;
            }
            Ok(MonitorPoll::Empty) => {
                self.monitor_pending = false;
                self.next_reconcile = Instant::now() + RECONCILE_TICK;
                self.monitor = match Monitor::new(MonitorMode::Enumerate) {
                    Ok(monitor) => Some(monitor),
                    Err(error) => {
                        self.emit(&format!("wiilandd: error: HID discovery: {error}"));
                        None
                    }
                };
            }
            Err(error) => {
                self.emit(&format!("wiilandd: error: HID discovery: {error}"));
                self.monitor = None;
                self.monitor_pending = false;
                self.next_reconcile = Instant::now() + RECONCILE_TICK;
            }
        }
    }

    fn service_capture_leases(&mut self) {
        let captures = self
            .ipc
            .as_ref()
            .map(IpcServer::capture_paths)
            .unwrap_or_default();
        let diagnostics = self.diagnostics.clone();
        for device in self.slots.iter_mut().flatten() {
            let capture = captures.iter().any(|path| Path::new(path) == device.path());
            if let Err(code) = device.set_capture(capture) {
                diagnostics.send(&format!(
                    "wiilandd: error: capture {}: {code}",
                    device.path().display()
                ));
            }
        }
    }

    fn control_command(
        &mut self,
        command: wiiland_ipc::Command,
    ) -> Result<wiiland_ipc::ResponseResult, wiiland_ipc::ProtocolError> {
        use wiiland_ipc::{Command, ProtocolErrorCode, ResponseResult};
        match command {
            Command::Diagnostics => {
                let mut metrics = self.metrics.clone();
                metrics.trace_records_dropped = self.trace_dropped.dropped();
                metrics.lifecycle_records_dropped = self.diagnostics.dropped();
                Ok(ResponseResult::Diagnostics(metrics))
            }
            Command::Config => Ok(ResponseResult::Config(self.config.dump())),
            Command::StartCapture { syspath } => {
                let slot =
                    self.find(Path::new(&syspath))
                        .ok_or_else(|| wiiland_ipc::ProtocolError {
                            code: ProtocolErrorCode::InvalidRequest,
                            message: "device is not owned by this daemon".into(),
                        })?;
                let device = self.slots[slot].as_mut().expect("found device");
                device
                    .set_capture(true)
                    .map_err(|code| wiiland_ipc::ProtocolError {
                        code: ProtocolErrorCode::Internal,
                        message: format!("cannot open capture interfaces: {code}"),
                    })?;
                Ok(ResponseResult::CaptureStarted(device.info()))
            }
            Command::StopCapture => Ok(ResponseResult::CaptureStopped),
            _ => Err(wiiland_ipc::ProtocolError {
                code: ProtocolErrorCode::UnknownCommand,
                message: "unsupported control command".into(),
            }),
        }
    }

    fn service_commands(&mut self) {
        let commands = self
            .ipc
            .as_mut()
            .map(IpcServer::take_commands)
            .unwrap_or_default();
        for (token, id, command) in commands {
            let result = self.control_command(command);
            if let Some(server) = self.ipc.as_mut() {
                server.complete_command(token, id, result);
            }
        }
        self.service_capture_leases();
    }

    fn status_snapshot(
        slots: &[Option<WindowsBridgeDevice>],
        dry_run: bool,
        path: &Path,
    ) -> Status {
        Status {
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            pid: std::process::id(),
            device_count: slots.iter().filter(|slot| slot.is_some()).count() as u32,
            dry_run,
            socket_path: path.to_string_lossy().into_owned(),
        }
    }

    fn device_snapshot(slots: &[Option<WindowsBridgeDevice>]) -> Vec<DeviceInfo> {
        slots
            .iter()
            .filter_map(|slot| slot.as_ref().map(WindowsBridgeDevice::info))
            .collect()
    }

    fn service_ipc(&mut self, token: u64) -> Result<(), RuntimeFailure> {
        let slots = &self.slots;
        let dry_run = self.dry_run;
        let mut status = |path: &Path| Self::status_snapshot(slots, dry_run, path);
        let mut devices = || Self::device_snapshot(slots);
        if let Some(server) = self.ipc.as_mut() {
            server
                .handle_ready(token, &mut status, &mut devices)
                .map_err(|error| RuntimeFailure::io("Windows IPC dispatch failed", error))?;
        }
        self.service_commands();
        Ok(())
    }

    fn wait_once(&mut self) -> Result<Option<WaitTarget>, RuntimeFailure> {
        self.wait_handles.clear();
        self.wait_targets.clear();
        self.wait_handles.push(self.stop_event.raw());
        self.wait_targets.push(WaitTarget::Stop);
        self.ipc_sources.clear();
        if let Some(server) = self.ipc.as_ref() {
            server.poll_sources(&mut self.ipc_sources);
        }
        let start_source = self.next_wait_source;
        for offset in 0..=MAX_DEVICES {
            let source_index = (start_source + offset) % (MAX_DEVICES + 1);
            if source_index == 0 {
                if let Some(source) = self.ipc_sources.first() {
                    self.wait_handles.push(source.handle);
                    self.wait_targets.push(WaitTarget::Ipc(source.token));
                }
            } else {
                let slot = source_index - 1;
                if let Some(device) = self.slots[slot].as_ref() {
                    self.wait_handles.push(device.wait_handle());
                    self.wait_targets.push(WaitTarget::Device(slot));
                }
            }
        }
        self.next_wait_source = (start_source + 1) % (MAX_DEVICES + 1);
        if self.wait_handles.len() > MAX_WAIT_HANDLES || self.wait_handles.len() > 64 {
            return Err(RuntimeFailure::new(
                -libc::EOVERFLOW,
                "wiilandd: Windows wait set exceeds the supported handle count",
            ));
        }
        let now = Instant::now();
        let deadline = self
            .next_pointer
            .map_or(self.next_output_refresh, |pointer| {
                pointer.min(self.next_output_refresh)
            });
        let timeout = deadline.saturating_duration_since(now);
        let timeout_ms = timeout.as_micros().div_ceil(1000).min(u32::MAX as u128 - 1) as u32;
        let result = unsafe {
            WaitForMultipleObjects(
                self.wait_handles.len() as u32,
                self.wait_handles.as_ptr(),
                0,
                timeout_ms,
            )
        };
        if result == WAIT_TIMEOUT {
            return Ok(None);
        }
        if result == WAIT_FAILED {
            return Err(RuntimeFailure::io(
                "WaitForMultipleObjects failed",
                io::Error::last_os_error(),
            ));
        }
        let index = result
            .checked_sub(WAIT_OBJECT_0)
            .map(|index| index as usize)
            .filter(|index| *index < self.wait_targets.len())
            .ok_or_else(|| {
                RuntimeFailure::new(
                    -libc::EIO,
                    format!("wiilandd: invalid Windows wait result {result}"),
                )
            })?;
        let target = self.wait_targets.get(index).copied().ok_or_else(|| {
            RuntimeFailure::new(
                -libc::EIO,
                format!("wiilandd: Windows wait returned out-of-range object {index}"),
            )
        })?;
        Ok(Some(target))
    }

    fn run(&mut self) -> Result<(), RuntimeFailure> {
        let mut initialized = false;
        loop {
            let target = self.wait_once()?;
            if matches!(target, Some(WaitTarget::Stop)) {
                return Ok(());
            }
            let work_started = Instant::now();
            if !initialized {
                if let Some(path) = self.selected_path.clone() {
                    self.open_device(&path).map_err(|code| {
                        RuntimeFailure::new(
                            code,
                            format!("wiilandd: cannot open device {}: {code}", path.display()),
                        )
                    })?;
                }
                initialized = true;
            }
            match target {
                Some(WaitTarget::Ipc(token)) => self.service_ipc(token)?,
                Some(WaitTarget::Device(slot)) => self.drain_device(slot),
                Some(WaitTarget::Stop) | None => {}
            }
            let now = Instant::now();
            self.service_timers(now);
            self.service_discovery(now);
            self.metrics.max_dispatch_duration_us = self.metrics.max_dispatch_duration_us.max(
                Instant::now()
                    .duration_since(work_started)
                    .as_micros()
                    .min(u64::MAX as u128) as u64,
            );
            if self.selected_path.is_some() && self.slots.iter().all(Option::is_none) {
                return Ok(());
            }
        }
    }

    fn service_timers(&mut self, now: Instant) {
        if let Some(next_pointer) = self.next_pointer.filter(|deadline| now >= *deadline) {
            let lateness = now
                .duration_since(next_pointer)
                .as_micros()
                .min(u64::MAX as u128) as u64;
            self.metrics.max_pointer_lateness_us =
                self.metrics.max_pointer_lateness_us.max(lateness);
            for slot in 0..MAX_DEVICES {
                if self.slots[slot]
                    .as_ref()
                    .is_some_and(WindowsBridgeDevice::pointer_active)
                    && let Some(Err(code)) = self.slots[slot]
                        .as_mut()
                        .map(WindowsBridgeDevice::tick_pointer)
                {
                    self.remove_device(
                        slot,
                        RemovalReason::PointerError,
                        Some("pointer tick"),
                        Some(code),
                    );
                }
            }
            self.next_pointer = advance_pointer_deadline(next_pointer, now);
        }
        if now >= self.next_output_refresh {
            self.service_output_workers();
            self.next_output_refresh = now + OUTPUT_REFRESH_TICK;
        }
    }

    fn service_output_workers(&mut self) {
        for slot in 0..MAX_DEVICES {
            let Some(status) = self.slots[slot]
                .as_ref()
                .and_then(WindowsBridgeDevice::output_status)
            else {
                continue;
            };
            let failure = match status {
                OutputWorkerStatus::Failed(code) => Some(code),
                OutputWorkerStatus::Finished => Some(-libc::EIO),
                OutputWorkerStatus::Stopping => Some(-libc::ECANCELED),
                OutputWorkerStatus::Starting | OutputWorkerStatus::Ready => None,
            };
            if let Some(code) = failure {
                self.remove_device(
                    slot,
                    RemovalReason::DrainError,
                    Some("output worker"),
                    Some(code),
                );
                continue;
            }
            if let Some(Err(code)) = self.slots[slot].as_mut().map(WindowsBridgeDevice::refresh) {
                self.remove_device(
                    slot,
                    RemovalReason::DrainError,
                    Some("output report refresh"),
                    Some(code),
                );
            }
        }
        self.reap_finished_output_workers();
    }
}

fn advance_pointer_deadline(deadline: Instant, now: Instant) -> Option<Instant> {
    let elapsed_nanos = now.duration_since(deadline).as_nanos();
    let tick_nanos = POINTER_TICK.as_nanos();
    let until_next_nanos = tick_nanos - elapsed_nanos % tick_nanos;
    let until_next = Duration::new(
        (until_next_nanos / 1_000_000_000) as u64,
        (until_next_nanos % 1_000_000_000) as u32,
    );
    now.checked_add(until_next)
}

impl Drop for WindowsRuntime {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            if let Some(mut device) = slot.take()
                && let Some(worker) = device.request_output_close()
            {
                self.retired_output_workers.push(worker);
            }
        }
        for worker in self.retired_output_workers.drain(..) {
            let _ = worker.join();
        }
        self.ipc.take();
    }
}

/// Enter the native Windows daemon runtime for a parsed `Action::Run` request.
pub(crate) fn run_cli(cli: &Cli) -> i32 {
    if cli.action != Action::Run {
        eprintln!("wiilandd: Windows runtime received a non-run action");
        return libc::EINVAL;
    }
    let mut runtime = match WindowsRuntime::new(cli) {
        Ok(runtime) => runtime,
        Err(failure) => {
            eprintln!("{}", failure.message);
            return exit_code(failure.code);
        }
    };
    match runtime.run() {
        Ok(()) => 0,
        Err(failure) => {
            eprintln!("{}", failure.message);
            exit_code(failure.code)
        }
    }
}
