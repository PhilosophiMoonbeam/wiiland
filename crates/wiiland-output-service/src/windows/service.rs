use crate::protocol::{
    MAX_SLOTS, Operation, OutputLease, REPORT_LAYOUT_VERSION, ReportId, Request, Response, winerr,
};
use crate::windows::driver::DeviceHandle;
use crate::windows::pipe::{OverlappedEvent, PIPE_INSTANCE_COUNT, Pipe};
use crate::windows::security::{self, OwnedHandle, PipeSecurity};
use std::ffi::{OsStr, c_void};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{GetLastError, HANDLE, NO_ERROR, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::EventLog::{
    CloseEventLog, EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE, RegisterEventSourceW,
    ReportEventW,
};
use windows_sys::Win32::System::RemoteDesktop::{
    WTSGetActiveConsoleSessionId, WTSSESSION_NOTIFICATION,
};
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_POWEREVENT, SERVICE_ACCEPT_SESSIONCHANGE,
    SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE,
    SERVICE_CONTROL_POWEREVENT, SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_SHUTDOWN,
    SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS,
    SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
    SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForSingleObject,
};

const SERVICE_NAME: &[u16] = &[
    87, 105, 105, 76, 97, 110, 100, 79, 117, 116, 112, 117, 116, 0,
];
const NO_SESSION: u32 = u32::MAX;
const SESSION_LOGON: u32 = 5;
const SESSION_LOGOFF: u32 = 6;
const SESSION_LOCK: u32 = 7;
const SESSION_UNLOCK: u32 = 8;
const SESSION_CONSOLE_CONNECT: u32 = 1;
const SESSION_CONSOLE_DISCONNECT: u32 = 2;
const POWER_SUSPEND: u32 = 0x0004;
const POWER_RESUME_CRITICAL: u32 = 0x0006;
const POWER_RESUME_SUSPEND: u32 = 0x0007;
const POWER_RESUME_AUTOMATIC: u32 = 0x0012;
const SERVICE_EVENT_ID: u32 = 0x2001;
const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: i32 = 1063;

struct ServiceState {
    stopping: AtomicBool,
    suspended: AtomicBool,
    driver_resetting: AtomicBool,
    active_session: AtomicU32,
    unlocked_session: AtomicU32,
    authorization_epoch: AtomicU64,
    session_update: Mutex<()>,
    session_notification_seen: AtomicBool,
    session_query_error_logged: AtomicBool,
    state_event: OwnedHandle,
    status_handle: AtomicPtr<c_void>,
}

impl ServiceState {
    fn new(event: HANDLE) -> Self {
        Self {
            stopping: AtomicBool::new(false),
            suspended: AtomicBool::new(false),
            driver_resetting: AtomicBool::new(false),
            active_session: AtomicU32::new(NO_SESSION),
            unlocked_session: AtomicU32::new(NO_SESSION),
            authorization_epoch: AtomicU64::new(1),
            session_update: Mutex::new(()),
            session_notification_seen: AtomicBool::new(false),
            session_query_error_logged: AtomicBool::new(false),
            state_event: OwnedHandle(event),
            status_handle: AtomicPtr::new(ptr::null_mut()),
        }
    }

    fn session_update_lock(&self) -> MutexGuard<'_, ()> {
        self.session_update
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn output_session_locked(&self) -> Option<u32> {
        if self.stopping.load(Ordering::Acquire)
            || self.suspended.load(Ordering::Acquire)
            || self.driver_resetting.load(Ordering::Acquire)
        {
            return None;
        }
        let active = self.active_session.load(Ordering::Acquire);
        let unlocked = self.unlocked_session.load(Ordering::Acquire);
        if active == NO_SESSION || active != unlocked {
            return None;
        }
        let console = unsafe { WTSGetActiveConsoleSessionId() };
        (console == active).then_some(active)
    }

    fn output_session(&self) -> Option<u32> {
        let _update = self.session_update_lock();
        self.output_session_locked()
    }

    fn output_epoch(&self) -> Option<(u32, u64)> {
        let _update = self.session_update_lock();
        self.output_session_locked()
            .map(|session| (session, self.authorization_epoch.load(Ordering::Acquire)))
    }

    fn is_epoch_output_allowed(&self, session: u32, epoch: u64) -> bool {
        let _update = self.session_update_lock();
        self.authorization_epoch.load(Ordering::Acquire) == epoch
            && self.output_session_locked() == Some(session)
            && self.authorization_epoch.load(Ordering::Acquire) == epoch
    }

    fn reset_event_for_epoch(&self, session: u32, epoch: u64) -> io::Result<bool> {
        let _update = self.session_update_lock();
        if self.authorization_epoch.load(Ordering::Acquire) != epoch
            || self.output_session_locked() != Some(session)
        {
            return Ok(false);
        }
        if unsafe { ResetEvent(self.state_event.0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(true)
    }

    fn reset_state_event(&self) -> io::Result<()> {
        let _update = self.session_update_lock();
        if unsafe { ResetEvent(self.state_event.0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn advance_epoch_locked(&self) {
        self.authorization_epoch.fetch_add(1, Ordering::AcqRel);
        unsafe { SetEvent(self.state_event.0) };
    }

    fn request_epoch_restart(&self) {
        let _update = self.session_update_lock();
        self.advance_epoch_locked();
    }

    fn request_driver_reset(&self) {
        let _update = self.session_update_lock();
        if !self.driver_resetting.swap(true, Ordering::AcqRel) {
            self.advance_epoch_locked();
        }
    }

    fn finish_driver_reset(&self) {
        let _update = self.session_update_lock();
        self.driver_resetting.store(false, Ordering::Release);
        self.advance_epoch_locked();
    }

    fn stop(&self) {
        let _update = self.session_update_lock();
        if !self.stopping.swap(true, Ordering::AcqRel) {
            self.advance_epoch_locked();
        }
    }

    fn seed_active_console(&self) {
        if self.stopping.load(Ordering::Acquire)
            || self.suspended.load(Ordering::Acquire)
            || self.session_notification_seen.load(Ordering::Acquire)
        {
            return;
        }
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if session == NO_SESSION {
            return;
        }
        let is_unlocked = match security::session_is_unlocked(session) {
            Ok(is_unlocked) => {
                self.session_query_error_logged
                    .store(false, Ordering::Release);
                is_unlocked
            }
            Err(error) => {
                if !self.session_query_error_logged.swap(true, Ordering::AcqRel) {
                    log_error(&format!(
                        "cannot query active console lock state; keeping output disabled until a WTS unlock event: {error}"
                    ));
                }
                return;
            }
        };

        let _update = self.session_update_lock();
        if !self.session_notification_seen.load(Ordering::Acquire)
            && !self.suspended.load(Ordering::Acquire)
            && !self.stopping.load(Ordering::Acquire)
        {
            self.active_session.store(session, Ordering::Release);
            self.unlocked_session.store(
                if is_unlocked { session } else { NO_SESSION },
                Ordering::Release,
            );
        }
    }
}

const REPORT_DEADLINE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
struct ReportFreshness {
    gamepad_deadline: Instant,
    supplemental_deadline: Instant,
}

impl ReportFreshness {
    fn new(now: Instant) -> Self {
        Self {
            gamepad_deadline: now + REPORT_DEADLINE,
            supplemental_deadline: now + REPORT_DEADLINE,
        }
    }

    fn refresh(&mut self, report_id: ReportId, now: Instant) {
        let deadline = now + REPORT_DEADLINE;
        match report_id {
            ReportId::Gamepad => self.gamepad_deadline = deadline,
            ReportId::SupplementalAxes => self.supplemental_deadline = deadline,
        }
    }

    fn refresh_if_fresh(&mut self, report_id: ReportId, now: Instant) -> bool {
        if self.expired(now) {
            return false;
        }
        self.refresh(report_id, now);
        true
    }

    fn next_deadline(self) -> Instant {
        self.gamepad_deadline.min(self.supplemental_deadline)
    }

    fn expired(self, now: Instant) -> bool {
        now >= self.gamepad_deadline || now >= self.supplemental_deadline
    }
}

#[derive(Clone, Copy)]
struct LeaseEntry {
    generation: u32,
    freshness: ReportFreshness,
}

#[derive(Clone, Copy)]
struct ConnectionLeases {
    leases: [Option<LeaseEntry>; MAX_SLOTS],
}

impl ConnectionLeases {
    const fn new() -> Self {
        Self {
            leases: [None; MAX_SLOTS],
        }
    }

    fn first_free_slot(&self) -> Option<usize> {
        self.leases.iter().position(Option::is_none)
    }

    fn next_report_deadline(&self) -> Option<Instant> {
        self.leases
            .iter()
            .flatten()
            .map(|lease| lease.freshness.next_deadline())
            .min()
    }

    fn report_deadline_expired(&self, now: Instant) -> bool {
        self.leases
            .iter()
            .flatten()
            .any(|lease| lease.freshness.expired(now))
    }
}

struct BrokerState {
    device: Option<DeviceHandle>,
    connections: [ConnectionLeases; PIPE_INSTANCE_COUNT],
}

struct SharedBroker(Mutex<BrokerState>);

impl SharedBroker {
    fn new(device: DeviceHandle) -> Self {
        Self(Mutex::new(BrokerState {
            device: Some(device),
            connections: [ConnectionLeases::new(); PIPE_INSTANCE_COUNT],
        }))
    }

    fn lock(&self) -> MutexGuard<'_, BrokerState> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn next_report_deadline(&self, connection: usize) -> Option<Instant> {
        self.lock().connections[connection].next_report_deadline()
    }

    fn report_deadline_expired(&self, connection: usize) -> bool {
        self.lock().connections[connection].report_deadline_expired(Instant::now())
    }

    fn reset_device(&self) -> io::Result<()> {
        let old_device = {
            let mut broker = self.lock();
            broker.connections = [ConnectionLeases::new(); PIPE_INSTANCE_COUNT];
            broker.device.take()
        };
        drop(old_device);
        let device = DeviceHandle::open()?;
        self.lock().device = Some(device);
        Ok(())
    }
}

pub(crate) fn run() -> io::Result<()> {
    let service_name = SERVICE_NAME.as_ptr().cast_mut();
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: service_name,
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        let code = unsafe { GetLastError() };
        if code as i32 == ERROR_FAILED_SERVICE_CONTROLLER_CONNECT {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "start `wiiland-output-service.exe` through the Windows Service Control Manager as the `WiiLandOutput` service; direct console execution is unsupported",
            ));
        }
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    Ok(())
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        log_error(&format!(
            "WiiLandOutput could not create its service state event: {}",
            io::Error::last_os_error()
        ));
        return;
    }
    let state = Box::new(ServiceState::new(event));
    let status_handle = unsafe {
        RegisterServiceCtrlHandlerExW(
            SERVICE_NAME.as_ptr(),
            Some(service_control),
            (&*state as *const ServiceState).cast(),
        )
    };
    if status_handle.is_null() {
        log_error(&format!(
            "WiiLandOutput could not register its SCM control handler: {}",
            io::Error::last_os_error()
        ));
        return;
    }
    state.status_handle.store(status_handle, Ordering::Release);
    set_status(&state, SERVICE_START_PENDING, 0, 1, 30_000);

    let result = (|| {
        security::verify_local_service_identity()?;
        set_status(&state, SERVICE_RUNNING, 0, 0, 0);
        state.seed_active_console();
        let broker = SharedBroker::new(DeviceHandle::open()?);
        log_information(
            "WiiLandOutput is running; output is restricted to the active, unlocked console session",
        );
        run_broker(&state, &broker)
    })();
    match result {
        Ok(()) => set_status(&state, SERVICE_STOPPED, 0, 0, 0),
        Err(error) => {
            log_error(&format!("WiiLandOutput stopped after an error: {error}"));
            let (win32_exit, service_exit) = match error.raw_os_error() {
                Some(code) if code > 0 => (code as u32, 0),
                _ => (winerr::ERROR_SERVICE_SPECIFIC_ERROR, 1),
            };
            set_status(&state, SERVICE_STOPPED, win32_exit, service_exit, 0);
        }
    }
}

unsafe extern "system" fn service_control(
    control: u32,
    event_type: u32,
    event_data: *mut c_void,
    context: *mut c_void,
) -> u32 {
    if context.is_null() {
        return NO_ERROR;
    }
    let state = unsafe { &*(context.cast::<ServiceState>()) };
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            state.stop();
            set_status(state, SERVICE_STOP_PENDING, 0, 0, 30_000);
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            if !event_data.is_null() {
                let notification = unsafe { &*event_data.cast::<WTSSESSION_NOTIFICATION>() };
                handle_session_change(state, event_type, notification.dwSessionId);
            }
        }
        SERVICE_CONTROL_POWEREVENT => handle_power_change(state, event_type),
        SERVICE_CONTROL_INTERROGATE => {
            if state.stopping.load(Ordering::Acquire) {
                set_status(state, SERVICE_STOP_PENDING, 0, 0, 30_000);
            } else {
                set_status(state, SERVICE_RUNNING, 0, 0, 0);
            }
        }
        _ => {}
    }
    NO_ERROR
}

fn handle_session_change(state: &ServiceState, event_type: u32, session: u32) {
    let _update = state.session_update_lock();
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    let active = state.active_session.load(Ordering::Acquire);
    let changed = match event_type {
        SESSION_LOGON if console == session => {
            state.active_session.store(session, Ordering::Release);
            state.unlocked_session.store(NO_SESSION, Ordering::Release);
            true
        }
        SESSION_UNLOCK if console == session => {
            state.active_session.store(session, Ordering::Release);
            state.unlocked_session.store(session, Ordering::Release);
            true
        }
        SESSION_LOCK if active == session || console == session => {
            state.active_session.store(session, Ordering::Release);
            state.unlocked_session.store(NO_SESSION, Ordering::Release);
            true
        }
        SESSION_LOGOFF | SESSION_CONSOLE_DISCONNECT if active == session || console == session => {
            state.unlocked_session.store(NO_SESSION, Ordering::Release);
            state.active_session.store(NO_SESSION, Ordering::Release);
            true
        }
        SESSION_CONSOLE_CONNECT if console == session => {
            state.active_session.store(session, Ordering::Release);
            state.unlocked_session.store(NO_SESSION, Ordering::Release);
            true
        }
        _ => false,
    };
    if changed {
        state
            .session_notification_seen
            .store(true, Ordering::Release);
        state.advance_epoch_locked();
    }
}

fn handle_power_change(state: &ServiceState, event_type: u32) {
    let _update = state.session_update_lock();
    match event_type {
        POWER_SUSPEND => {
            state.suspended.store(true, Ordering::Release);
            state.unlocked_session.store(NO_SESSION, Ordering::Release);
        }
        POWER_RESUME_AUTOMATIC | POWER_RESUME_SUSPEND | POWER_RESUME_CRITICAL => {
            state.suspended.store(false, Ordering::Release);
            state.unlocked_session.store(NO_SESSION, Ordering::Release);
            state
                .session_notification_seen
                .store(false, Ordering::Release);
        }
        _ => return,
    }
    state.advance_epoch_locked();
}

fn set_status(
    state: &ServiceState,
    current_state: u32,
    win32_exit: u32,
    service_exit: u32,
    wait_hint: u32,
) {
    let handle = state.status_handle.load(Ordering::Acquire);
    if handle.is_null() {
        return;
    }
    let accepted = if current_state == SERVICE_RUNNING {
        SERVICE_ACCEPT_STOP
            | SERVICE_ACCEPT_SHUTDOWN
            | SERVICE_ACCEPT_SESSIONCHANGE
            | SERVICE_ACCEPT_POWEREVENT
    } else {
        0
    };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: current_state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: win32_exit,
        dwServiceSpecificExitCode: service_exit,
        dwCheckPoint: if current_state == SERVICE_START_PENDING
            || current_state == SERVICE_STOP_PENDING
        {
            1
        } else {
            0
        },
        dwWaitHint: wait_hint,
    };
    unsafe { SetServiceStatus(handle, &status) };
}

fn run_broker(state: &ServiceState, broker: &SharedBroker) -> io::Result<()> {
    let system_security = security::system_pipe_security()?;
    let mut pipes = Vec::with_capacity(PIPE_INSTANCE_COUNT);
    for index in 0..PIPE_INSTANCE_COUNT {
        pipes.push(Pipe::create(&system_security, index == 0)?);
    }
    let mut events = Vec::with_capacity(PIPE_INSTANCE_COUNT);
    for _ in 0..PIPE_INSTANCE_COUNT {
        events.push(OverlappedEvent::new()?);
    }

    loop {
        if state.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        if state.driver_resetting.load(Ordering::Acquire) {
            restrict_pipe_pool(state, &pipes, &system_security)?;
            if state.stopping.load(Ordering::Acquire) {
                return Ok(());
            }
            broker.reset_device()?;
            state.finish_driver_reset();
        }

        let Some(waited_session) = wait_for_unlocked_console(state)? else {
            return Ok(());
        };
        let Some((session_id, epoch)) = state.output_epoch() else {
            continue;
        };
        if session_id != waited_session {
            continue;
        }
        let identity = match security::active_logon_identity(session_id) {
            Ok(identity) => identity,
            Err(error) => {
                log_error(&format!("cannot acquire active console identity: {error}"));
                wait_for_state_change(state, 1_000)?;
                continue;
            }
        };
        if !state.is_epoch_output_allowed(session_id, epoch) {
            continue;
        }

        let active_security = security::pipe_security(&identity)?;
        if let Err(error) = set_pipe_pool_security(&pipes, &active_security) {
            log_error(&format!(
                "cannot authorize retained WiiLandOutput pipe instances: {error}"
            ));
            restrict_pipe_pool(state, &pipes, &system_security)?;
            wait_for_state_change(state, 1_000)?;
            continue;
        }
        if !state.reset_event_for_epoch(session_id, epoch)? {
            restrict_pipe_pool(state, &pipes, &system_security)?;
            continue;
        }

        let spawn_error = thread::scope(|scope| {
            let mut workers = Vec::with_capacity(PIPE_INSTANCE_COUNT);
            let mut spawn_error = None;
            for connection in 0..PIPE_INSTANCE_COUNT {
                let worker_pipe = &pipes[connection];
                let worker_event = &events[connection];
                let worker_identity = &identity;
                let spawn = thread::Builder::new()
                    .name(format!("WiiLandOutput pipe {connection}"))
                    .spawn_scoped(scope, move || {
                        let worker = ConnectionWorker {
                            state,
                            broker,
                            connection,
                            pipe: worker_pipe,
                            event: worker_event,
                            identity: worker_identity,
                            session_id,
                            epoch,
                        };
                        let result =
                            catch_unwind(AssertUnwindSafe(|| run_connection_worker(worker)));
                        match result {
                            Ok(Ok(()))
                                if !state.stopping.load(Ordering::Acquire)
                                    && state.is_epoch_output_allowed(session_id, epoch) =>
                            {
                                state.request_epoch_restart();
                            }
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => {
                                log_error(&format!(
                                    "WiiLandOutput connection worker {connection} failed: {error}"
                                ));
                                state.request_epoch_restart();
                            }
                            Err(_) => {
                                log_error(&format!(
                                    "WiiLandOutput connection worker {connection} panicked"
                                ));
                                state.request_driver_reset();
                            }
                        }
                    });
                match spawn {
                    Ok(worker) => workers.push(worker),
                    Err(error) => {
                        spawn_error = Some(error);
                        state.request_epoch_restart();
                        break;
                    }
                }
            }
            for worker in workers {
                if worker.join().is_err() {
                    state.request_driver_reset();
                }
            }
            spawn_error
        });
        if let Some(error) = spawn_error {
            log_error(&format!(
                "could not start all WiiLandOutput pipe workers: {error}"
            ));
        }
        for pipe in &pipes {
            pipe.disconnect()?;
        }
        if state.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        restrict_pipe_pool(state, &pipes, &system_security)?;
        if state.driver_resetting.load(Ordering::Acquire) {
            broker.reset_device()?;
            state.finish_driver_reset();
        }
    }
}

fn set_pipe_pool_security(pipes: &[Pipe], security: &PipeSecurity) -> io::Result<()> {
    for pipe in pipes {
        pipe.set_security(security)?;
    }
    Ok(())
}

fn restrict_pipe_pool(
    state: &ServiceState,
    pipes: &[Pipe],
    system_security: &PipeSecurity,
) -> io::Result<()> {
    loop {
        let mut first_error = None;
        for pipe in pipes {
            if let Err(error) = pipe.set_security(system_security)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        let Some(error) = first_error else {
            return Ok(());
        };
        log_error(&format!(
            "could not restrict the retained WiiLandOutput pipe namespace to SYSTEM: {error}"
        ));
        if state.stopping.load(Ordering::Acquire) {
            return Err(error);
        }
        wait_for_state_change(state, 1_000)?;
    }
}

#[derive(Clone, Copy)]
struct ConnectionWorker<'a> {
    state: &'a ServiceState,
    broker: &'a SharedBroker,
    connection: usize,
    pipe: &'a Pipe,
    event: &'a OverlappedEvent,
    identity: &'a security::LogonIdentity,
    session_id: u32,
    epoch: u64,
}

fn run_connection_worker(worker: ConnectionWorker<'_>) -> io::Result<()> {
    let ConnectionWorker {
        state,
        broker,
        connection,
        pipe,
        event,
        session_id,
        epoch,
        ..
    } = worker;
    while state.is_epoch_output_allowed(session_id, epoch) {
        if broker.report_deadline_expired(connection) {
            log_error(&format!(
                "WiiLandOutput connection {connection} exceeded a report freshness deadline"
            ));
            break;
        }
        if !pipe.connect(event, state.state_event.0)? {
            break;
        }
        let served = catch_unwind(AssertUnwindSafe(|| serve_connected(worker)));
        cleanup_connection(state, broker, connection);
        pipe.disconnect()?;
        if served.is_err() {
            log_error(&format!(
                "WiiLandOutput connection worker {connection} panicked while serving a client"
            ));
            state.request_epoch_restart();
            return Err(io::Error::other(
                "WiiLandOutput connection worker panicked while serving a client",
            ));
        }
    }
    Ok(())
}

fn serve_connected(worker: ConnectionWorker<'_>) {
    let ConnectionWorker {
        state,
        broker,
        connection,
        pipe,
        event,
        identity,
        session_id,
        epoch,
    } = worker;
    let mut abi_handshaken = false;
    loop {
        if !state.is_epoch_output_allowed(session_id, epoch) {
            break;
        }
        if broker.report_deadline_expired(connection) {
            log_error(&format!(
                "WiiLandOutput connection {connection} exceeded a report freshness deadline"
            ));
            break;
        }
        let report_deadline = broker.next_report_deadline(connection);
        let bytes = match pipe.read_request(event, state.state_event.0, report_deadline) {
            Ok(bytes) => bytes,
            Err(error) => {
                if broker.report_deadline_expired(connection) {
                    log_error(&format!(
                        "WiiLandOutput connection {connection} exceeded a report freshness deadline"
                    ));
                } else if error.kind() != io::ErrorKind::Interrupted
                    && error.kind() != io::ErrorKind::UnexpectedEof
                    && state.is_epoch_output_allowed(session_id, epoch)
                {
                    log_error(&format!("WiiLandOutput pipe read failed: {error}"));
                }
                break;
            }
        };
        if !state.is_epoch_output_allowed(session_id, epoch)
            || broker.report_deadline_expired(connection)
        {
            break;
        }
        if let Err(error) = security::verify_client(pipe.raw(), identity) {
            log_error(&format!(
                "rejected changed WiiLandOutput pipe peer: {error}"
            ));
            break;
        }
        let request = match Request::decode(&bytes) {
            Ok(request) => request,
            Err(code) => {
                log_error(&format!(
                    "rejected malformed WiiLandOutput request (Win32 error {code})"
                ));
                break;
            }
        };
        if !state.is_epoch_output_allowed(session_id, epoch) {
            break;
        }
        if broker.report_deadline_expired(connection) {
            log_error(&format!(
                "WiiLandOutput connection {connection} exceeded a report freshness deadline"
            ));
            break;
        }
        if !abi_handshaken && request.operation != Operation::Heartbeat {
            log_error("rejected WiiLandOutput request before the v3 broker handshake");
            break;
        }
        let response = dispatch_request(state, broker, connection, session_id, epoch, request);
        if request.operation == Operation::Heartbeat && response.status == 0 {
            abi_handshaken = true;
        }
        if !state.is_epoch_output_allowed(session_id, epoch)
            || broker.report_deadline_expired(connection)
        {
            break;
        }
        let unauthorized_lease =
            matches!(request.operation, Operation::Report | Operation::Destroy)
                && response.status == winerr::ERROR_INVALID_HANDLE;
        let response_bytes = response.encode();
        if let Err(error) = pipe.write_response(
            &response_bytes,
            event,
            state.state_event.0,
            broker.next_report_deadline(connection),
        ) {
            if error.kind() != io::ErrorKind::Interrupted
                && state.is_epoch_output_allowed(session_id, epoch)
            {
                log_error(&format!("WiiLandOutput pipe response failed: {error}"));
            }
            break;
        }
        if broker.report_deadline_expired(connection) {
            log_error(&format!(
                "WiiLandOutput connection {connection} exceeded a report freshness deadline"
            ));
            break;
        }
        if unauthorized_lease {
            break;
        }
    }
}

fn wait_for_unlocked_console(state: &ServiceState) -> io::Result<Option<u32>> {
    loop {
        if state.stopping.load(Ordering::Acquire) {
            return Ok(None);
        }
        state.seed_active_console();
        if let Some(session) = state.output_session() {
            return Ok(Some(session));
        }
        let result = unsafe { WaitForSingleObject(state.state_event.0, 5_000) };
        if result == WAIT_OBJECT_0 {
            state.reset_state_event()?;
        } else if result != WAIT_TIMEOUT {
            return Err(io::Error::last_os_error());
        }
    }
}

fn wait_for_state_change(state: &ServiceState, milliseconds: u32) -> io::Result<()> {
    let result = unsafe { WaitForSingleObject(state.state_event.0, milliseconds) };
    if result == WAIT_OBJECT_0 {
        state.reset_state_event()?;
        return Ok(());
    }
    if result == WAIT_TIMEOUT {
        return Ok(());
    }
    Err(io::Error::last_os_error())
}

fn dispatch_request(
    state: &ServiceState,
    broker: &SharedBroker,
    connection_index: usize,
    session_id: u32,
    epoch: u64,
    request: Request,
) -> Response {
    let mut response = Response {
        operation: request.operation,
        status: 0,
        slot: request.slot,
        generation: request.generation,
        report_layout_version: REPORT_LAYOUT_VERSION,
    };
    if !state.is_epoch_output_allowed(session_id, epoch) {
        response.status = winerr::ERROR_NOT_READY;
        return response;
    }
    let mut broker = broker.lock();
    if !state.is_epoch_output_allowed(session_id, epoch) {
        response.status = winerr::ERROR_NOT_READY;
        return response;
    }
    if broker.connections[connection_index].report_deadline_expired(Instant::now()) {
        response.status = winerr::ERROR_NOT_READY;
        return response;
    }
    let BrokerState {
        device,
        connections,
    } = &mut *broker;
    let connection = &mut connections[connection_index];
    match request.operation {
        Operation::Heartbeat => {}
        Operation::Create => {
            if connection.first_free_slot().is_none() {
                response.status = winerr::ERROR_NOT_ENOUGH_QUOTA;
                return response;
            }
            let Some(device) = device.as_ref() else {
                response.status = winerr::ERROR_NOT_READY;
                return response;
            };
            let mut globally_occupied = false;
            for slot in 0..MAX_SLOTS {
                if connection.leases[slot].is_some() {
                    continue;
                }
                match device.create_slot(slot as u32) {
                    Ok(lease) => {
                        connection.leases[slot] = Some(LeaseEntry {
                            generation: lease.generation,
                            freshness: ReportFreshness::new(Instant::now()),
                        });
                        if !state.is_epoch_output_allowed(session_id, epoch) {
                            match device.neutralize_and_destroy(lease) {
                                Ok(()) => connection.leases[slot] = None,
                                Err(error) => {
                                    log_error(&format!(
                                        "could not tear down a lease created during an epoch change: {error}"
                                    ));
                                    state.request_driver_reset();
                                }
                            }
                            response.status = winerr::ERROR_NOT_READY;
                            return response;
                        }
                        response.slot = lease.slot;
                        response.generation = lease.generation;
                        return response;
                    }
                    Err(error) if error.raw_os_error() == Some(winerr::ERROR_BUSY as i32) => {
                        globally_occupied = true;
                    }
                    Err(error) => {
                        response.status = error_code(&error);
                        return response;
                    }
                }
            }
            response.status = if globally_occupied {
                winerr::ERROR_BUSY
            } else {
                winerr::ERROR_NOT_ENOUGH_QUOTA
            };
        }
        Operation::Report => {
            let slot = request.slot as usize;
            let Some(entry) = connection.leases.get_mut(slot).and_then(Option::as_mut) else {
                response.status = winerr::ERROR_INVALID_HANDLE;
                return response;
            };
            if entry.generation != request.generation {
                response.status = winerr::ERROR_INVALID_HANDLE;
                return response;
            }
            let report_id = match ReportId::try_from(request.report_id) {
                Ok(report_id) => report_id,
                Err(_) => {
                    response.status = winerr::ERROR_INVALID_PARAMETER;
                    return response;
                }
            };
            let lease = OutputLease {
                slot: request.slot,
                generation: request.generation,
            };
            let Some(device) = device.as_ref() else {
                response.status = winerr::ERROR_NOT_READY;
                return response;
            };
            // The session callback cannot revoke this epoch halfway through a
            // driver submission; it advances the epoch under the same gate.
            let _epoch_gate = state.session_update_lock();
            if state.authorization_epoch.load(Ordering::Acquire) != epoch
                || state.output_session_locked() != Some(session_id)
                || entry.freshness.expired(Instant::now())
            {
                response.status = winerr::ERROR_NOT_READY;
                return response;
            }
            match device.report_payload(
                lease,
                report_id as u8,
                &request.payload[..request.payload_len as usize],
            ) {
                Ok(()) => {
                    if !entry.freshness.refresh_if_fresh(report_id, Instant::now()) {
                        response.status = winerr::ERROR_NOT_READY;
                    }
                }
                Err(error) => response.status = error_code(&error),
            }
        }
        Operation::Destroy => {
            let slot = request.slot as usize;
            let Some(entry) = connection.leases.get(slot).and_then(Option::as_ref) else {
                response.status = winerr::ERROR_INVALID_HANDLE;
                return response;
            };
            if entry.generation != request.generation {
                response.status = winerr::ERROR_INVALID_HANDLE;
                return response;
            }
            let lease = OutputLease {
                slot: request.slot,
                generation: request.generation,
            };
            let Some(device) = device.as_ref() else {
                response.status = winerr::ERROR_NOT_READY;
                return response;
            };
            match device.neutralize_and_destroy(lease) {
                Ok(()) => connection.leases[slot] = None,
                Err(error) => response.status = error_code(&error),
            }
        }
    }
    response
}

fn cleanup_connection(state: &ServiceState, broker: &SharedBroker, connection_index: usize) {
    let mut failures = Vec::new();
    {
        let mut broker = broker.lock();
        let BrokerState {
            device,
            connections,
        } = &mut *broker;
        let connection = &mut connections[connection_index];
        for slot in 0..MAX_SLOTS {
            let Some(entry) = connection.leases[slot] else {
                continue;
            };
            let result = match device.as_ref() {
                Some(device) => device.neutralize_and_destroy(OutputLease {
                    slot: slot as u32,
                    generation: entry.generation,
                }),
                None => Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "shared driver handle is unavailable during lease cleanup",
                )),
            };
            match result {
                Ok(()) => connection.leases[slot] = None,
                Err(error) => failures.push((slot, error)),
            }
        }
        if !failures.is_empty() {
            state.request_driver_reset();
        }
    }
    if !failures.is_empty() {
        for (slot, error) in failures {
            log_error(&format!(
                "could not destroy output lease for connection {connection_index}, slot {slot}; invalidating the shared driver handle: {error}"
            ));
        }
    }
}

fn error_code(error: &io::Error) -> u32 {
    error
        .raw_os_error()
        .and_then(|code| u32::try_from(code).ok())
        .filter(|code| *code != 0)
        .unwrap_or(winerr::ERROR_INVALID_DATA)
}

fn log_information(message: &str) {
    write_event(EVENTLOG_INFORMATION_TYPE, message);
}

fn log_error(message: &str) {
    write_event(EVENTLOG_ERROR_TYPE, message);
}

fn write_event(event_type: u16, message: &str) {
    let source = SERVICE_NAME.as_ptr();
    let event_log = unsafe { RegisterEventSourceW(ptr::null(), source) };
    if event_log.is_null() {
        return;
    }
    let text: Vec<u16> = OsStr::new(message).encode_wide().chain(Some(0)).collect();
    let strings = [text.as_ptr()];
    unsafe {
        ReportEventW(
            event_log,
            event_type,
            0,
            SERVICE_EVENT_ID,
            ptr::null_mut(),
            1,
            0,
            strings.as_ptr(),
            ptr::null(),
        );
        CloseEventLog(event_log);
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_deadlines_start_at_create_and_refresh_independently() {
        let created = Instant::now();
        let mut freshness = ReportFreshness::new(created);
        assert!(!freshness.expired(created + Duration::from_millis(1_999)));
        assert!(freshness.expired(created + REPORT_DEADLINE));

        freshness = ReportFreshness::new(created);
        freshness.refresh(ReportId::Gamepad, created + Duration::from_millis(500));
        assert!(!freshness.expired(created + Duration::from_millis(1_999)));
        assert!(freshness.expired(created + REPORT_DEADLINE));

        freshness = ReportFreshness::new(created);
        freshness.refresh(
            ReportId::SupplementalAxes,
            created + Duration::from_millis(500),
        );
        assert!(!freshness.expired(created + Duration::from_millis(1_999)));
        assert!(freshness.expired(created + REPORT_DEADLINE));
    }

    #[test]
    fn successful_reports_move_only_their_own_deadline() {
        let created = Instant::now();
        let mut freshness = ReportFreshness::new(created);
        freshness.refresh(ReportId::Gamepad, created + Duration::from_millis(500));
        freshness.refresh(
            ReportId::SupplementalAxes,
            created + Duration::from_millis(700),
        );

        assert!(!freshness.expired(created + Duration::from_millis(2_499)));
        assert!(freshness.expired(created + Duration::from_millis(2_500)));
        assert_eq!(
            freshness.next_deadline(),
            created + Duration::from_millis(2_500)
        );
    }

    #[test]
    fn completed_report_cannot_resurrect_an_expired_lease() {
        let created = Instant::now();
        let mut freshness = ReportFreshness::new(created);
        assert!(!freshness.refresh_if_fresh(ReportId::Gamepad, created + REPORT_DEADLINE));
        assert_eq!(freshness.next_deadline(), created + REPORT_DEADLINE);

        let mut freshness = ReportFreshness::new(created);
        assert!(freshness.refresh_if_fresh(
            ReportId::Gamepad,
            created + REPORT_DEADLINE - Duration::from_nanos(1),
        ));
        assert_eq!(freshness.supplemental_deadline, created + REPORT_DEADLINE);
    }

    #[test]
    fn connection_and_lease_tables_keep_their_fixed_capacity() {
        let mut connections = [ConnectionLeases::new(); PIPE_INSTANCE_COUNT];
        assert_eq!(connections.len(), 32);
        assert!(
            connections
                .iter()
                .all(|connection| connection.leases.len() == 32)
        );
        assert!(
            connections
                .iter()
                .all(|connection| connection.first_free_slot() == Some(0))
        );
        assert_eq!(connections[0].next_report_deadline(), None);
        assert!(!connections[0].report_deadline_expired(Instant::now()));

        for slot in 0..MAX_SLOTS {
            assert_eq!(connections[0].first_free_slot(), Some(slot));
            connections[0].leases[slot] = Some(LeaseEntry {
                generation: slot as u32 + 1,
                freshness: ReportFreshness::new(Instant::now()),
            });
        }
        assert_eq!(connections[0].first_free_slot(), None);
        assert_eq!(connections[1].first_free_slot(), Some(0));
        connections[0].leases[MAX_SLOTS - 1] = None;
        assert_eq!(connections[0].first_free_slot(), Some(MAX_SLOTS - 1));
    }
}
