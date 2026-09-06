//! Single-thread monitor/device reactor.
use crate::bridge::{BridgeAction, BridgeDevice};
use crate::diagnostics::DiagnosticWriter;
use crate::ipc::IpcServer;
use crate::signal::SignalPipe;
use crate::uinput::{Backend, SystemBackend};
use std::cell::Cell;
use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};
use wiiland_core::{Config, Profile, TraceConfig};
use wiiland_hid::{
    Axis3 as Abs, Button, ButtonEvent as HidButtonEvent, ButtonState, Event, EventKind, Monitor,
    MonitorMode,
};
use wiiland_ipc::{
    Axis3, ButtonEvent, DeviceInfo, InputPayload, Notification, RemovalReason, Status, Timestamp,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PollOwner {
    Signal,
    Monitor,
    Device(usize),
    Ipc(u64),
}

fn has_ready_ipc(poll_owners: &[PollOwner], poll_fds: &[libc::pollfd]) -> bool {
    debug_assert_eq!(poll_owners.len(), poll_fds.len());
    poll_owners
        .iter()
        .zip(poll_fds)
        .any(|(owner, fd)| matches!(owner, PollOwner::Ipc(_)) && fd.revents != 0)
}

fn should_collect_input(has_input_subscribers: Option<bool>) -> bool {
    has_input_subscribers == Some(true)
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

fn button_code(button: Button) -> Option<u32> {
    Some(button.code())
}

fn button_state(state: ButtonState) -> Option<u32> {
    Some(state.value())
}

fn button(value: HidButtonEvent) -> Option<ButtonEvent> {
    Some(ButtonEvent {
        code: button_code(value.button)?,
        state: button_state(value.state)?,
    })
}

fn input_payload(kind: EventKind) -> InputPayload {
    let raw = event_type_code(kind);
    match kind {
        EventKind::Key(value) => {
            button(value).map_or(InputPayload::Unknown(raw), InputPayload::Key)
        }
        EventKind::Accel(value) => InputPayload::Accel(axis(value)),
        EventKind::Ir(values) => InputPayload::Ir(values.map(axis)),
        EventKind::BalanceBoard(values) => InputPayload::BalanceBoard(values.map(axis)),
        EventKind::MotionPlus(value) => InputPayload::MotionPlus(axis(value)),
        EventKind::ProControllerKey(value) => {
            button(value).map_or(InputPayload::Unknown(raw), InputPayload::ProControllerKey)
        }
        EventKind::ProControllerMove(values) => InputPayload::ProControllerMove(values.map(axis)),
        EventKind::Watch => InputPayload::Watch,
        EventKind::ClassicControllerKey(value) => button(value).map_or(
            InputPayload::Unknown(raw),
            InputPayload::ClassicControllerKey,
        ),
        EventKind::ClassicControllerMove(values) => {
            InputPayload::ClassicControllerMove(values.map(axis))
        }
        EventKind::NunchukKey(value) => {
            button(value).map_or(InputPayload::Unknown(raw), InputPayload::NunchukKey)
        }
        EventKind::NunchukMove(values) => InputPayload::NunchukMove(values.map(axis)),
        EventKind::DrumsKey(value) => {
            button(value).map_or(InputPayload::Unknown(raw), InputPayload::DrumsKey)
        }
        EventKind::DrumsMove(values) => InputPayload::DrumsMove(values.map(axis)),
        EventKind::GuitarKey(value) => {
            button(value).map_or(InputPayload::Unknown(raw), InputPayload::GuitarKey)
        }
        EventKind::GuitarMove(values) => InputPayload::GuitarMove(values.map(axis)),
        EventKind::Gone => InputPayload::Gone,
        EventKind::Unknown(value) => InputPayload::Unknown(value),
        other => InputPayload::Unknown(event_type_code(other)),
    }
}

fn timestamp(event: &Event) -> Timestamp {
    Timestamp {
        seconds: event.time.seconds,
        micros: event.time.microseconds,
    }
}

fn ipc_profile(profile: Profile) -> wiiland_ipc::Profile {
    if profile == Profile::GAMEPAD {
        wiiland_ipc::Profile::Gamepad
    } else if profile == Profile::DESKTOP {
        wiiland_ipc::Profile::Desktop
    } else if profile == Profile::BOTH {
        wiiland_ipc::Profile::Both
    } else {
        wiiland_ipc::Profile::None
    }
}

pub const MAX_DEVICES: usize = 32;
pub const POINTER_TICK: Duration = Duration::from_micros(16_000);
pub const RECONCILE_TICK: Duration = Duration::from_secs(1);

type DiagnosticSink = Box<dyn FnMut(&str)>;

enum Lifecycle<'a> {
    Add(&'a Path),
    Remove(&'a Path),
    Gone(&'a Path),
    Reconcile {
        snapshot: usize,
        queued: usize,
        active: usize,
    },
    Error {
        operation: &'static str,
        path: Option<&'a Path>,
        code: i32,
    },
}

impl fmt::Display for Lifecycle<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Add(path) => write!(f, "wiilandd: add: {}", path.display()),
            Self::Remove(path) => write!(f, "wiilandd: remove: {}", path.display()),
            Self::Gone(path) => write!(f, "wiilandd: gone: {}", path.display()),
            Self::Reconcile {
                snapshot,
                queued,
                active,
            } => write!(
                f,
                "wiilandd: reconcile: snapshot={snapshot} queued={queued} active={active}"
            ),
            Self::Error {
                operation,
                path: Some(path),
                code,
            } => write!(f, "wiilandd: error: {operation} {}: {code}", path.display()),
            Self::Error {
                operation,
                path: None,
                code,
            } => write!(f, "wiilandd: error: {operation}: {code}"),
        }
    }
}

struct Diagnostics {
    enabled: bool,
    sink: DiagnosticSink,
}

impl Diagnostics {
    fn queued(writer: &DiagnosticWriter) -> Self {
        let sender = writer.sender();
        Self {
            enabled: false,
            sink: Box::new(move |line| sender.send(line)),
        }
    }

    fn emit(&mut self, event: Lifecycle<'_>) {
        if self.enabled {
            (self.sink)(&event.to_string());
        }
    }
}

fn outputs_enabled(dry_run: bool) -> bool {
    !dry_run
}

fn io_errno(error: &io::Error) -> i32 {
    -error.raw_os_error().unwrap_or(libc::EIO)
}

fn event_type_code(kind: EventKind) -> u32 {
    kind.event_type().code()
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) -> bool {
    if paths.contains(&path) {
        false
    } else {
        paths.push(path);
        true
    }
}

fn missing_slots<'a>(
    active: impl Iterator<Item = (usize, &'a Path)>,
    present: &[PathBuf],
) -> Vec<usize> {
    active
        .filter_map(|(slot, path)| (!present.iter().any(|item| item == path)).then_some(slot))
        .collect()
}

pub struct Runtime<B: Backend + Clone = SystemBackend> {
    config: Config,
    slots: [Option<BridgeDevice<B>>; MAX_DEVICES],
    monitor: Option<Monitor>,
    signal: SignalPipe,
    backend: B,
    dry_run: bool,
    diagnostics: Diagnostics,
    trace: TraceConfig,
    trace_sequence: Rc<Cell<u64>>,
    notification_sequence: Rc<Cell<u64>>,
    ipc: Option<IpcServer>,
    poll_fds: Vec<libc::pollfd>,
    poll_owners: Vec<PollOwner>,
    ipc_sources: Vec<crate::ipc::PollSource>,
    trace_writer: DiagnosticWriter,
    diagnostic_writer: DiagnosticWriter,
    metrics: wiiland_ipc::Diagnostics,
}

fn device_info<B: Backend + Clone>(dev: &BridgeDevice<B>) -> DeviceInfo {
    DeviceInfo {
        syspath: dev.path().to_string_lossy().into_owned(),
        profile: ipc_profile(dev.profile),
        opened_interfaces: dev.opened_ifaces.bits(),
        pending_interfaces: dev.pending_ifaces.bits(),
        gamepad_output: dev.gamepad.is_some(),
        desktop_output: dev.desktop.is_some(),
    }
}

fn status_snapshot<B: Backend + Clone>(
    slots: &[Option<BridgeDevice<B>>],
    dry_run: bool,
    socket_path: &Path,
) -> Status {
    Status {
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
        device_count: slots.iter().filter(|slot| slot.is_some()).count() as u32,
        dry_run,
        socket_path: socket_path.to_string_lossy().into_owned(),
    }
}

fn device_snapshot<B: Backend + Clone>(slots: &[Option<BridgeDevice<B>>]) -> Vec<DeviceInfo> {
    slots
        .iter()
        .filter_map(|slot| slot.as_ref().map(device_info))
        .collect()
}

impl Runtime<SystemBackend> {
    pub fn new(config: Config) -> Result<Self, i32> {
        Self::with_backend(config, SystemBackend)
    }
}
impl<B: Backend + Clone> Runtime<B> {
    pub fn with_backend(config: Config, backend: B) -> Result<Self, i32> {
        config.validate().map_err(|_| -libc::EINVAL)?;
        let diagnostic_writer = DiagnosticWriter::stderr();
        Ok(Self {
            config,
            slots: std::array::from_fn(|_| None),
            monitor: None,
            signal: SignalPipe::install()?,
            backend,
            dry_run: false,
            diagnostics: Diagnostics::queued(&diagnostic_writer),
            trace: TraceConfig::default(),
            trace_sequence: Rc::new(Cell::new(0)),
            notification_sequence: Rc::new(Cell::new(0)),
            ipc: None,
            poll_fds: Vec::with_capacity(MAX_DEVICES + 2),
            poll_owners: Vec::with_capacity(MAX_DEVICES + 2),
            ipc_sources: Vec::new(),
            trace_writer: DiagnosticWriter::stdout(),
            diagnostic_writer,
            metrics: wiiland_ipc::Diagnostics::default(),
        })
    }
    pub fn enable_ipc(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let server = IpcServer::bind(path.as_ref())?;
        self.ipc = Some(server);
        Ok(())
    }
    fn publish(&mut self, notification: Notification) {
        if let Some(server) = self.ipc.as_mut() {
            server.publish(notification);
        }
    }
    fn publish_added(&mut self, info: DeviceInfo) {
        let sequence = next_sequence(&self.notification_sequence);
        self.publish(Notification::DeviceAdded {
            sequence,
            device: info,
        });
    }
    fn publish_removed(&mut self, path: &Path, reason: RemovalReason) {
        let sequence = next_sequence(&self.notification_sequence);
        self.publish(Notification::DeviceRemoved {
            sequence,
            syspath: path.to_string_lossy().into_owned(),
            reason,
        });
    }
    pub fn set_dry_run(&mut self, value: bool) {
        self.dry_run = value;
    }
    pub fn set_verbose(&mut self, value: bool) {
        self.diagnostics.enabled = value;
    }
    pub fn set_trace(&mut self, value: TraceConfig) {
        self.trace = value;
    }
    pub fn add_path(&mut self, path: impl AsRef<Path>) -> Result<bool, i32> {
        let path = path.as_ref();
        if self.find(path).is_some() {
            return Ok(false);
        }
        let Some(slot) = self.slots.iter().position(Option::is_none) else {
            let code = -libc::ENOSPC;
            self.diagnostics.emit(Lifecycle::Error {
                operation: "add",
                path: Some(path),
                code,
            });
            return Err(code);
        };
        match BridgeDevice::with_backend_outputs(
            path,
            &self.config,
            self.backend.clone(),
            outputs_enabled(self.dry_run),
        ) {
            Ok(mut dev) => {
                if self.trace.enabled {
                    let sender = self.trace_writer.sender();
                    dev.set_trace_sink_with_sequence(
                        self.trace.filter,
                        Rc::clone(&self.trace_sequence),
                        move |line| sender.send(line),
                    );
                }
                self.slots[slot] = Some(dev);
                let info = self.slots[slot]
                    .as_ref()
                    .map(device_info)
                    .expect("just inserted");
                self.publish_added(info);
                self.diagnostics.emit(Lifecycle::Add(path));
                Ok(true)
            }
            Err(code) => {
                self.diagnostics.emit(Lifecycle::Error {
                    operation: "add",
                    path: Some(path),
                    code,
                });
                Err(code)
            }
        }
    }
    fn find(&self, path: &Path) -> Option<usize> {
        self.slots
            .iter()
            .position(|x| x.as_ref().is_some_and(|d| d.path() == path))
    }
    fn reconcile(&mut self) {
        let mut snapshot = match Monitor::new(MonitorMode::Enumerate) {
            Ok(monitor) => monitor,
            Err(error) => {
                self.diagnostics.emit(Lifecycle::Error {
                    operation: "reconcile snapshot",
                    path: None,
                    code: io_errno(&error),
                });
                return;
            }
        };
        let mut paths = Vec::<PathBuf>::new();
        loop {
            match snapshot.poll() {
                Ok(Some(path)) => {
                    push_unique(&mut paths, path);
                }
                Ok(None) => break,
                Err(error) => {
                    self.diagnostics.emit(Lifecycle::Error {
                        operation: "reconcile snapshot",
                        path: None,
                        code: io_errno(&error),
                    });
                    return;
                }
            }
        }
        let snapshot_count = paths.len();
        let remove = missing_slots(
            self.slots
                .iter()
                .enumerate()
                .filter_map(|(slot, dev)| dev.as_ref().map(|dev| (slot, dev.path()))),
            &paths,
        );
        for slot in remove {
            if let Some(dev) = self.slots[slot].take() {
                let path = dev.path().to_path_buf();
                self.publish_removed(&path, RemovalReason::Removed);
                self.diagnostics.emit(Lifecycle::Remove(&path));
            }
        }
        for path in paths {
            let _ = self.add_path(path);
        }

        let mut queued = Vec::new();
        let mut queued_count = 0;
        if let Some(mon) = self.monitor.as_mut() {
            loop {
                match mon.poll() {
                    Ok(Some(path)) => {
                        queued_count += 1;
                        push_unique(&mut queued, path);
                    }
                    Ok(None) => break,
                    Err(error) => {
                        self.diagnostics.emit(Lifecycle::Error {
                            operation: "monitor",
                            path: None,
                            code: io_errno(&error),
                        });
                        break;
                    }
                }
            }
        }
        for path in queued {
            let _ = self.add_path(path);
        }
        let active = self.slots.iter().filter(|slot| slot.is_some()).count();
        self.diagnostics.emit(Lifecycle::Reconcile {
            snapshot: snapshot_count,
            queued: queued_count,
            active,
        });
    }
    fn drain_device(&mut self, slot: usize, single: bool) -> Result<(), i32> {
        let Some(path) = self.slots[slot]
            .as_ref()
            .map(|dev| dev.path().to_path_buf())
        else {
            return Ok(());
        };
        let outcome =
            if should_collect_input(self.ipc.as_ref().map(IpcServer::has_input_subscribers)) {
                let sequence = Rc::clone(&self.notification_sequence);
                let syspath = path.to_string_lossy().into_owned();
                let mut inputs = Vec::new();
                let outcome = self.slots[slot].as_mut().map(|dev| {
                    dev.drain_with(|_, event| {
                        inputs.push(Notification::Input {
                            sequence: next_sequence(&sequence),
                            syspath: syspath.clone(),
                            timestamp: timestamp(event),
                            payload: input_payload(event.kind),
                        });
                    })
                });
                for notification in inputs {
                    self.publish(notification);
                }
                outcome
            } else {
                self.slots[slot].as_mut().map(BridgeDevice::drain)
            };
        match outcome {
            Some(Ok(BridgeAction::Continue)) | None => {}
            Some(Ok(BridgeAction::Gone)) => {
                if self.slots[slot].take().is_some() {
                    self.publish_removed(&path, RemovalReason::Gone);
                    self.diagnostics.emit(Lifecycle::Gone(&path));
                }
            }
            Some(Err(code)) => {
                if self.slots[slot].take().is_some() {
                    self.publish_removed(&path, RemovalReason::DrainError);
                    self.diagnostics.emit(Lifecycle::Error {
                        operation: "drain",
                        path: Some(&path),
                        code,
                    });
                }
                if single {
                    return Err(code);
                }
            }
        }
        Ok(())
    }
    fn poll_once(&mut self, timeout_ms: i32, single: bool) -> Result<bool, i32> {
        self.poll_fds.clear();
        self.poll_owners.clear();
        self.ipc_sources.clear();
        self.poll_fds.push(libc::pollfd {
            fd: self.signal.read_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        self.poll_owners.push(PollOwner::Signal);
        if let Some(mon) = self.monitor.as_mut()
            && let Some(fd) = mon.fd()
        {
            self.poll_fds.push(libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            self.poll_owners.push(PollOwner::Monitor);
        }
        for (slot, dev) in self.slots.iter().enumerate() {
            if let Some(dev) = dev {
                self.poll_fds.push(libc::pollfd {
                    fd: dev.iface.as_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
                self.poll_owners.push(PollOwner::Device(slot));
            }
        }
        if let Some(server) = self.ipc.as_mut() {
            server.poll_sources(&mut self.ipc_sources);
        }
        for source in &self.ipc_sources {
            self.poll_fds.push(libc::pollfd {
                fd: source.fd,
                events: source.events,
                revents: 0,
            });
            self.poll_owners.push(PollOwner::Ipc(source.token));
        }

        let result = unsafe {
            libc::poll(
                self.poll_fds.as_mut_ptr(),
                self.poll_fds.len() as libc::nfds_t,
                timeout_ms,
            )
        };
        if result < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                return Ok(false);
            }
            let code = io_errno(&e);
            self.diagnostics.emit(Lifecycle::Error {
                operation: "poll",
                path: None,
                code,
            });
            return Err(code);
        }
        if self.signal.requested() || self.poll_fds[0].revents != 0 {
            let _ = self.signal.drain();
            return Ok(true);
        }
        if result == 0 {
            return Ok(false);
        }

        let dispatch_started = Instant::now();
        // Slots never move, so all device owners remain valid until this phase
        // is complete. Reconciliation is deliberately deferred below.
        for index in 0..self.poll_owners.len() {
            if self.poll_fds[index].revents == 0 {
                continue;
            }
            if let PollOwner::Device(slot) = self.poll_owners[index] {
                self.drain_device(slot, single)?;
            }
        }

        let monitor_ready = self.poll_owners.iter().enumerate().any(|(index, owner)| {
            *owner == PollOwner::Monitor && self.poll_fds[index].revents != 0
        });
        if monitor_ready {
            self.reconcile();
        }

        if has_ready_ipc(&self.poll_owners, &self.poll_fds) {
            let slots = &self.slots;
            let dry_run = &self.dry_run;
            let mut status = |socket_path: &Path| status_snapshot(slots, *dry_run, socket_path);
            let mut devices = || device_snapshot(slots);
            for index in 0..self.poll_owners.len() {
                if self.poll_fds[index].revents == 0 {
                    continue;
                }
                let PollOwner::Ipc(token) = self.poll_owners[index] else {
                    continue;
                };
                let Some(server) = self.ipc.as_mut() else {
                    continue;
                };
                if let Err(error) = server.handle_ready(
                    token,
                    self.poll_fds[index].revents,
                    &mut status,
                    &mut devices,
                ) {
                    let code = io_errno(&error);
                    self.diagnostics.emit(Lifecycle::Error {
                        operation: "ipc",
                        path: None,
                        code,
                    });
                    return Err(code);
                }
            }
        }
        self.service_commands();
        self.metrics.max_dispatch_duration_us = self
            .metrics
            .max_dispatch_duration_us
            .max(dispatch_started.elapsed().as_micros().min(u64::MAX as u128) as u64);
        Ok(false)
    }
    fn control_command(
        &mut self,
        command: wiiland_ipc::Command,
    ) -> Result<wiiland_ipc::ResponseResult, wiiland_ipc::ProtocolError> {
        use wiiland_ipc::{Command, ProtocolError, ProtocolErrorCode, ResponseResult};
        match command {
            Command::Diagnostics => {
                let mut metrics = self.metrics.clone();
                metrics.trace_records_dropped = self.trace_writer.sender().dropped();
                metrics.lifecycle_records_dropped = self.diagnostic_writer.sender().dropped();
                Ok(ResponseResult::Diagnostics(metrics))
            }
            Command::Config => Ok(ResponseResult::Config(self.config.dump())),
            Command::StartCapture { syspath } => {
                let slot = self
                    .find(Path::new(&syspath))
                    .ok_or_else(|| ProtocolError {
                        code: ProtocolErrorCode::InvalidRequest,
                        message: "device is not owned by this daemon".into(),
                    })?;
                let device = self.slots[slot].as_mut().expect("found device");
                device.set_capture(true).map_err(|code| ProtocolError {
                    code: ProtocolErrorCode::Internal,
                    message: format!("cannot open capture interfaces: {code}"),
                })?;
                Ok(ResponseResult::CaptureStarted(device_info(device)))
            }
            Command::StopCapture => Ok(ResponseResult::CaptureStopped),
            _ => Err(ProtocolError {
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
        let captures = self
            .ipc
            .as_ref()
            .map(IpcServer::capture_paths)
            .unwrap_or_default();
        for device in self.slots.iter_mut().flatten() {
            let capture = captures.iter().any(|path| Path::new(path) == device.path());
            if let Err(code) = device.set_capture(capture) {
                self.diagnostics.emit(Lifecycle::Error {
                    operation: "capture",
                    path: Some(device.path()),
                    code,
                });
            }
        }
    }

    fn loop_run(&mut self, single: bool) -> Result<(), i32> {
        let mut next_pointer = Instant::now() + POINTER_TICK;
        let mut next_reconcile = Instant::now() + RECONCILE_TICK;
        loop {
            if self.signal.requested() {
                return Ok(());
            }
            let now = Instant::now();
            let mut deadline = next_pointer.min(next_reconcile);
            if single {
                deadline = next_pointer;
            }
            let timeout = deadline.saturating_duration_since(now);
            let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
            if self.poll_once(ms, single)? {
                return Ok(());
            }
            let now = Instant::now();
            if now >= next_pointer {
                self.metrics.max_pointer_lateness_us = self.metrics.max_pointer_lateness_us.max(
                    now.duration_since(next_pointer)
                        .as_micros()
                        .min(u64::MAX as u128) as u64,
                );
                for slot in 0..MAX_DEVICES {
                    let active = self.slots[slot]
                        .as_ref()
                        .is_some_and(|dev| dev.pointer_active());
                    if active {
                        let result = self.slots[slot].as_mut().map(|dev| dev.tick_pointer());
                        if let Some(Err(code)) = result {
                            if let Some(dev) = self.slots[slot].take() {
                                let path = dev.path().to_path_buf();
                                self.publish_removed(&path, RemovalReason::PointerError);
                                self.diagnostics.emit(Lifecycle::Error {
                                    operation: "pointer tick",
                                    path: Some(&path),
                                    code,
                                });
                            }
                            if single {
                                return Err(code);
                            }
                        }
                    }
                }
                while next_pointer <= now {
                    next_pointer += POINTER_TICK;
                }
            }
            if !single && now >= next_reconcile {
                self.reconcile();
                while next_reconcile <= now {
                    next_reconcile += RECONCILE_TICK;
                }
            }
            if single && self.slots.iter().all(Option::is_none) {
                return Ok(());
            }
        }
    }
    pub fn run_monitor(&mut self) -> Result<(), i32> {
        let mon = Monitor::new(MonitorMode::Watch).map_err(|error| {
            let code = io_errno(&error);
            self.diagnostics.emit(Lifecycle::Error {
                operation: "monitor",
                path: None,
                code,
            });
            code
        })?;
        self.monitor = Some(mon);
        self.reconcile();
        self.loop_run(false)
    }
    pub fn run_single(&mut self, path: impl AsRef<Path>) -> Result<(), i32> {
        self.add_path(path)?;
        self.loop_run(true)
    }
}
impl<B: Backend + Clone> Drop for Runtime<B> {
    fn drop(&mut self) {
        self.ipc.take();
        for slot in &mut self.slots {
            slot.take();
        }
        self.monitor.take();
        self.diagnostics.sink = Box::new(|_| {});
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn hid_and_wire_event_codes_agree_for_every_report() {
        let axis = Abs::default();
        let key = HidButtonEvent {
            button: Button::A,
            state: ButtonState::Pressed,
        };
        let reports = [
            EventKind::Key(key),
            EventKind::Accel(axis),
            EventKind::Ir([axis; 4]),
            EventKind::BalanceBoard([axis; 4]),
            EventKind::MotionPlus(axis),
            EventKind::ProControllerKey(key),
            EventKind::ProControllerMove([axis; 2]),
            EventKind::Watch,
            EventKind::ClassicControllerKey(key),
            EventKind::ClassicControllerMove([axis; 3]),
            EventKind::NunchukKey(key),
            EventKind::NunchukMove([axis; 2]),
            EventKind::DrumsKey(key),
            EventKind::DrumsMove([axis; 8]),
            EventKind::GuitarKey(key),
            EventKind::GuitarMove([axis; 3]),
            EventKind::Gone,
        ];
        for (code, report) in reports.into_iter().enumerate() {
            assert_eq!(report.event_type().code(), code as u32);
            assert_eq!(input_payload(report).event_code(), code as u32);
        }
    }

    #[test]
    fn empty_event_queue_does_not_remove_snapshot_devices() {
        let path = PathBuf::from("/sys/devices/wiimote0");
        let present = vec![path.clone()];
        let active = [(0, path.as_path())];

        assert!(missing_slots(active.into_iter(), &present).is_empty());
    }

    #[test]
    fn snapshot_removals_and_queued_additions_reconcile_independently() {
        let retained = PathBuf::from("/sys/devices/wiimote0");
        let removed = PathBuf::from("/sys/devices/wiimote1");
        let snapshot_add = PathBuf::from("/sys/devices/wiimote2");
        let queued_add = PathBuf::from("/sys/devices/wiimote3");
        let snapshot = vec![retained.clone(), snapshot_add.clone()];
        let mut queued = Vec::new();

        assert!(push_unique(&mut queued, queued_add.clone()));
        assert!(!push_unique(&mut queued, queued_add.clone()));

        let active = [(0, retained.as_path()), (1, removed.as_path())];
        assert_eq!(missing_slots(active.into_iter(), &snapshot), vec![1]);
        assert_eq!(snapshot, vec![retained, snapshot_add]);
        assert_eq!(queued, vec![queued_add]);
    }

    #[test]
    fn dry_run_disables_bridge_outputs() {
        assert!(outputs_enabled(false));
        assert!(!outputs_enabled(true));
    }

    #[test]
    fn lifecycle_diagnostics_are_quiet_unless_verbose_and_deterministic() {
        let lines = Rc::new(RefCell::new(Vec::<String>::new()));
        let captured = Rc::clone(&lines);
        let mut diagnostics = Diagnostics {
            enabled: false,
            sink: Box::new(move |line| captured.borrow_mut().push(line.to_owned())),
        };
        let path = Path::new("/sys/devices/wiimote0");

        diagnostics.emit(Lifecycle::Add(path));
        assert!(lines.borrow().is_empty());

        diagnostics.enabled = true;
        diagnostics.emit(Lifecycle::Add(path));
        diagnostics.emit(Lifecycle::Remove(path));
        diagnostics.emit(Lifecycle::Gone(path));
        diagnostics.emit(Lifecycle::Reconcile {
            snapshot: 2,
            queued: 1,
            active: 1,
        });
        diagnostics.emit(Lifecycle::Error {
            operation: "drain",
            path: Some(path),
            code: -libc::EIO,
        });
        diagnostics.emit(Lifecycle::Error {
            operation: "monitor",
            path: None,
            code: -libc::ENOMEM,
        });

        assert_eq!(
            *lines.borrow(),
            [
                "wiilandd: add: /sys/devices/wiimote0",
                "wiilandd: remove: /sys/devices/wiimote0",
                "wiilandd: gone: /sys/devices/wiimote0",
                "wiilandd: reconcile: snapshot=2 queued=1 active=1",
                "wiilandd: error: drain /sys/devices/wiimote0: -5",
                "wiilandd: error: monitor: -12",
            ]
        );
    }
    #[test]
    fn notification_sequence_wrap_skips_zero() {
        let sequence = Cell::new(u64::MAX);
        assert_eq!(next_sequence(&sequence), 1);
        assert_eq!(next_sequence(&sequence), 2);
    }

    #[test]
    fn profiles_map_to_protocol_names() {
        assert_eq!(ipc_profile(Profile::GAMEPAD), wiiland_ipc::Profile::Gamepad);
        assert_eq!(ipc_profile(Profile::DESKTOP), wiiland_ipc::Profile::Desktop);
        assert_eq!(ipc_profile(Profile::BOTH), wiiland_ipc::Profile::Both);
        assert_eq!(
            ipc_profile(Profile::default()),
            wiiland_ipc::Profile::Gamepad
        );
    }

    #[test]
    fn button_codes_and_states_preserve_wire_values() {
        let buttons = [
            (Button::Left, 0),
            (Button::Right, 1),
            (Button::Up, 2),
            (Button::Down, 3),
            (Button::A, 4),
            (Button::B, 5),
            (Button::Plus, 6),
            (Button::Minus, 7),
            (Button::Home, 8),
            (Button::One, 9),
            (Button::Two, 10),
            (Button::X, 11),
            (Button::Y, 12),
            (Button::ShoulderLeft, 13),
            (Button::ShoulderRight, 14),
            (Button::TriggerLeft, 15),
            (Button::TriggerRight, 16),
            (Button::ThumbLeft, 17),
            (Button::ThumbRight, 18),
            (Button::C, 19),
            (Button::Z, 20),
            (Button::StrumBarUp, 21),
            (Button::StrumBarDown, 22),
            (Button::FretFarUp, 23),
            (Button::FretUp, 24),
            (Button::FretMid, 25),
            (Button::FretLow, 26),
            (Button::FretFarLow, 27),
        ];
        assert_eq!(buttons.len(), 28);
        for (value, wire) in buttons {
            assert_eq!(button_code(value), Some(wire));
        }

        for (value, wire) in [
            (ButtonState::Released, 0),
            (ButtonState::Pressed, 1),
            (ButtonState::Repeated, 2),
        ] {
            assert_eq!(button_state(value), Some(wire));
        }
    }

    #[test]
    fn input_collection_requires_a_server_with_subscribers() {
        for (subscribers, expected) in [(None, false), (Some(false), false), (Some(true), true)] {
            assert_eq!(should_collect_input(subscribers), expected);
        }
    }

    #[test]
    fn snapshot_builders_preserve_daemon_dtos() {
        let slots: [Option<BridgeDevice<SystemBackend>>; MAX_DEVICES] =
            std::array::from_fn(|_| None);
        let socket_path = Path::new("/run/user/1000/wiiland/wiilandd.sock");

        let status = status_snapshot(&slots, true, socket_path);
        assert_eq!(status.daemon_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(status.pid, std::process::id());
        assert_eq!(status.device_count, 0);
        assert!(status.dry_run);
        assert_eq!(status.socket_path, socket_path.to_string_lossy());
        assert!(device_snapshot(&slots).is_empty());
    }

    #[test]
    fn ipc_dispatch_requires_a_ready_ipc_owner() {
        let owners = [
            PollOwner::Signal,
            PollOwner::Monitor,
            PollOwner::Device(0),
            PollOwner::Ipc(7),
        ];
        for (revents, expected) in [
            ([0, libc::POLLIN, 0, 0], false),
            ([0, 0, libc::POLLIN, 0], false),
            ([0, libc::POLLIN, libc::POLLIN, 0], false),
            ([0, 0, 0, libc::POLLIN], true),
            ([0, libc::POLLIN, libc::POLLIN, libc::POLLIN], true),
        ] {
            let poll_fds = revents.map(|revents| libc::pollfd {
                fd: -1,
                events: libc::POLLIN,
                revents,
            });
            assert_eq!(has_ready_ipc(&owners, &poll_fds), expected);
        }

        let owners = [PollOwner::Signal, PollOwner::Monitor, PollOwner::Device(0)];
        let poll_fds = [libc::pollfd {
            fd: -1,
            events: libc::POLLIN,
            revents: libc::POLLIN,
        }; 3];
        assert!(!has_ready_ipc(&owners, &poll_fds));
    }

    #[test]
    fn every_event_kind_maps_to_owned_payload_shape() {
        let axis_value = Abs { x: 1, y: 2, z: 3 };
        let key = HidButtonEvent {
            button: Button::A,
            state: ButtonState::Pressed,
        };
        assert!(matches!(
            input_payload(EventKind::Key(key)),
            InputPayload::Key(_)
        ));
        assert!(matches!(
            input_payload(EventKind::Accel(axis_value)),
            InputPayload::Accel(_)
        ));
        assert!(matches!(
            input_payload(EventKind::Ir([axis_value; 4])),
            InputPayload::Ir(values) if values.len() == 4
        ));
        assert!(matches!(
            input_payload(EventKind::BalanceBoard([axis_value; 4])),
            InputPayload::BalanceBoard(values) if values.len() == 4
        ));
        assert!(matches!(
            input_payload(EventKind::MotionPlus(axis_value)),
            InputPayload::MotionPlus(_)
        ));
        assert!(matches!(
            input_payload(EventKind::ProControllerKey(key)),
            InputPayload::ProControllerKey(_)
        ));
        assert!(matches!(
            input_payload(EventKind::ProControllerMove([axis_value; 2])),
            InputPayload::ProControllerMove(values) if values.len() == 2
        ));
        assert!(matches!(
            input_payload(EventKind::Watch),
            InputPayload::Watch
        ));
        assert!(matches!(
            input_payload(EventKind::ClassicControllerKey(key)),
            InputPayload::ClassicControllerKey(_)
        ));
        assert!(matches!(
            input_payload(EventKind::ClassicControllerMove([axis_value; 3])),
            InputPayload::ClassicControllerMove(values) if values.len() == 3
        ));
        assert!(matches!(
            input_payload(EventKind::NunchukKey(key)),
            InputPayload::NunchukKey(_)
        ));
        assert!(matches!(
            input_payload(EventKind::NunchukMove([axis_value; 2])),
            InputPayload::NunchukMove(values) if values.len() == 2
        ));
        assert!(matches!(
            input_payload(EventKind::DrumsKey(key)),
            InputPayload::DrumsKey(_)
        ));
        assert!(matches!(
            input_payload(EventKind::DrumsMove([axis_value; 8])),
            InputPayload::DrumsMove(values) if values.len() == 8
        ));
        assert!(matches!(
            input_payload(EventKind::GuitarKey(key)),
            InputPayload::GuitarKey(_)
        ));
        assert!(matches!(
            input_payload(EventKind::GuitarMove([axis_value; 3])),
            InputPayload::GuitarMove(values) if values.len() == 3
        ));
        assert!(matches!(input_payload(EventKind::Gone), InputPayload::Gone));
        assert!(matches!(
            input_payload(EventKind::Unknown(99)),
            InputPayload::Unknown(99)
        ));
    }
}
