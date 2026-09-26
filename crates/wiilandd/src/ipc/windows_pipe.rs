//! Windows named-pipe IPC server using overlapped I/O and one shared IOCP dispatcher.
#![allow(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::slice;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_IO_PENDING, ERROR_NOT_FOUND,
    ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    LocalFree, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, IsValidSid, RevertToSelf, SECURITY_ATTRIBUTES, TOKEN_GROUPS,
    TOKEN_QUERY, TOKEN_USER, TokenLogonSid, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED,
    PostQueuedCompletionStatus,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, ImpersonateNamedPipeClient,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentThread, INFINITE, OpenProcessToken, OpenThreadToken,
    ResetEvent, SetEvent, WaitForSingleObject,
};

use wiiland_ipc::windows_bootstrap::{
    BOOTSTRAP_ACCESS_MASK, BOOTSTRAP_DEADLINE, ClientAnnounce, ClientEcho, DaemonHello,
    DaemonReady, LogonSid, READY_STATUS_OK, ReturnPipeId, derive_return_pipe_name,
};

use super::{READ_CHUNK, WRITE_BUDGET};

const ENDPOINT_PREFIX: &str = r"\\.\pipe\WiiLand.";
const ENDPOINT_SUFFIX: &str = ".daemon";
const LOGON_ID_GROUP_ATTRIBUTES: u32 = 0xC000_0000;
const SE_GROUP_ENABLED: u32 = 0x0000_0004;
const SHUTDOWN_COMPLETION_KEY: usize = usize::MAX;
const TIMER_UPDATE_COMPLETION_KEY: usize = usize::MAX - 1;
const CLIENT_ANNOUNCE_BYTES: usize = 20;
const DAEMON_HELLO_BYTES: usize = 20;
const CLIENT_ECHO_BYTES: usize = 20;
const DAEMON_READY_BYTES: usize = 8;

#[derive(Debug)]
pub(super) enum PipeCompletion {
    Connected {
        token: u64,
        result: io::Result<()>,
    },
    Read {
        token: u64,
        bytes: usize,
        result: io::Result<()>,
    },
    Write {
        token: u64,
        bytes: usize,
        result: io::Result<()>,
    },
    BootstrapReady {
        token: u64,
    },
}

#[derive(Clone, Copy)]
struct RawCompletion {
    token: usize,
    overlapped: usize,
    bytes: u32,
    error: u32,
}

struct CompletionSignal {
    event: Handle,
    queue: Mutex<VecDeque<RawCompletion>>,
    deadline: Mutex<Option<Instant>>,
}

struct Handle(HANDLE);

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    fn raw(&self) -> HANDLE {
        self.0
    }

    fn into_raw(self) -> HANDLE {
        let raw = self.0;
        std::mem::forget(self);
        raw
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct Operation {
    overlapped: Box<std::cell::UnsafeCell<OVERLAPPED>>,
    pending: bool,
}

impl Operation {
    fn new() -> Self {
        Self {
            overlapped: Box::new(std::cell::UnsafeCell::new(OVERLAPPED::default())),
            pending: false,
        }
    }

    fn pointer(&self) -> *mut OVERLAPPED {
        self.overlapped.get()
    }

    fn reset(&self) {
        unsafe { *self.overlapped.get() = OVERLAPPED::default() };
    }
}

struct PipeInstance {
    handle: Handle,
    connect: Operation,
    read: Operation,
    write: Operation,
    read_buffer: Box<[u8; READ_CHUNK]>,
}

impl PipeInstance {
    fn new(handle: HANDLE) -> Self {
        Self {
            handle: Handle(handle),
            connect: Operation::new(),
            read: Operation::new(),
            write: Operation::new(),
            read_buffer: Box::new([0; READ_CHUNK]),
        }
    }

    fn pending_count(&self) -> usize {
        usize::from(self.connect.pending)
            + usize::from(self.read.pending)
            + usize::from(self.write.pending)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BootstrapPhase {
    ReadingAnnounce,
    WritingHello,
    ReadingEcho,
    WritingReady,
    Cancelling,
}

struct Bootstrap {
    token: u64,
    deadline: Instant,
    phase: BootstrapPhase,
    return_pipe: Option<PipeInstance>,
    read_offset: usize,
    challenge: [u8; 16],
    outbound: Box<[u8; DAEMON_HELLO_BYTES]>,
    outbound_len: usize,
    outbound_offset: usize,
}

impl Bootstrap {
    fn new(token: u64) -> Self {
        Self {
            token,
            deadline: Instant::now() + BOOTSTRAP_DEADLINE,
            phase: BootstrapPhase::ReadingAnnounce,
            return_pipe: None,
            challenge: [0; 16],
            read_offset: 0,
            outbound: Box::new([0; DAEMON_HELLO_BYTES]),
            outbound_len: 0,
            outbound_offset: 0,
        }
    }
}

/// Owns the permanent rendezvous instance, authenticated return-pipe handshakes,
/// active application pipes, and a single IOCP worker.
pub(super) struct WindowsPipeServer {
    wide_path: Vec<u16>,
    security: PipeSecurity,
    identity: CurrentIdentity,
    port: Handle,
    signal: Arc<CompletionSignal>,
    worker: Option<JoinHandle<()>>,
    listener: Option<(u64, PipeInstance)>,
    bootstrap: Option<Bootstrap>,
    clients: HashMap<u64, PipeInstance>,
}
impl WindowsPipeServer {
    pub(super) fn bind(path: &Path, listener_token: u64) -> io::Result<Self> {
        let identity = CurrentIdentity::open()?;
        let expected = endpoint_path(&identity.logon_sid_name);
        if path != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows IPC must use the current logon-session pipe endpoint",
            ));
        }
        let security = PipeSecurity::new(&identity)?;
        let wide_path = nul_terminated_wide(path);
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1) };
        if port.is_null() || port == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let port = Handle(port);
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let signal = Arc::new(CompletionSignal {
            event: Handle(event),
            queue: Mutex::new(VecDeque::new()),
            deadline: Mutex::new(None),
        });
        let worker_signal = Arc::clone(&signal);
        let worker_port = port.raw() as usize;
        let worker = thread::Builder::new()
            .name("wiiland-ipc-iocp".to_owned())
            .spawn(move || completion_worker(worker_port as HANDLE, worker_signal))?;
        let mut server = Self {
            wide_path,
            security,
            identity,
            port,
            signal,
            worker: Some(worker),
            listener: None,
            bootstrap: None,
            clients: HashMap::new(),
        };
        if let Err(error) = server.listen(listener_token) {
            drop(server);
            return Err(error);
        }
        Ok(server)
    }

    pub(super) fn completion_handle(&self) -> HANDLE {
        self.signal.event.raw()
    }

    pub(super) fn listener_token(&self) -> Option<u64> {
        self.listener.as_ref().map(|(token, _)| *token)
    }

    /// Creates the daemon's sole, permanent B instance. It is reused for every
    /// attempt; only this first creation claims FILE_FLAG_FIRST_PIPE_INSTANCE.
    pub(super) fn listen(&mut self, token: u64) -> io::Result<()> {
        if self.listener.is_some() {
            return Ok(());
        }
        let handle = unsafe {
            CreateNamedPipeW(
                self.wide_path.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                WRITE_BUDGET as u32,
                READ_CHUNK as u32,
                0,
                &self.security.attributes,
            )
        };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let handle = Handle(handle);
        let associated =
            unsafe { CreateIoCompletionPort(handle.raw(), self.port.raw(), token as usize, 0) };
        if associated.is_null() || associated != self.port.raw() {
            return Err(io::Error::last_os_error());
        }
        self.listener = Some((token, PipeInstance::new(handle.into_raw())));
        self.connect_listener(token)
    }

    fn connect_listener(&mut self, token: u64) -> io::Result<()> {
        let Some((listener_token, pipe)) = self.listener.as_mut() else {
            return Err(io::Error::other(
                "named-pipe rendezvous instance is missing",
            ));
        };
        if *listener_token != token || pipe.connect.pending || pipe.pending_count() != 0 {
            return Err(io::Error::other(
                "named-pipe rendezvous instance is not ready to connect",
            ));
        }
        pipe.connect.reset();
        pipe.connect.pending = true;
        if unsafe { ConnectNamedPipe(pipe.handle.raw(), pipe.connect.pointer()) } == 0 {
            let error = unsafe { GetLastError() };
            if error == ERROR_PIPE_CONNECTED {
                if unsafe {
                    PostQueuedCompletionStatus(
                        self.port.raw(),
                        0,
                        token as usize,
                        pipe.connect.pointer(),
                    )
                } == 0
                {
                    pipe.connect.pending = false;
                    return Err(io::Error::last_os_error());
                }
            } else if error != ERROR_IO_PENDING {
                pipe.connect.pending = false;
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        Ok(())
    }

    pub(super) fn reject_listener(&mut self, token: u64) -> io::Result<()> {
        self.reset_listener(token)
    }

    fn reset_listener(&mut self, token: u64) -> io::Result<()> {
        let Some((listener_token, pipe)) = self.listener.as_ref() else {
            return Err(io::Error::other(
                "named-pipe rendezvous instance is missing",
            ));
        };
        if *listener_token != token || pipe.pending_count() != 0 {
            return Err(io::Error::other(
                "named-pipe rendezvous instance is not quiescent",
            ));
        }
        if unsafe { DisconnectNamedPipe(pipe.handle.raw()) } == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_PIPE_NOT_CONNECTED {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        self.connect_listener(token)
    }

    pub(super) fn begin_bootstrap(
        &mut self,
        listener_token: u64,
        client_token: u64,
    ) -> io::Result<()> {
        let Some((actual_token, pipe)) = self.listener.as_ref() else {
            return Err(io::Error::other(
                "named-pipe connect completed without a listener",
            ));
        };
        if *actual_token != listener_token
            || pipe.connect.pending
            || pipe.pending_count() != 0
            || self.bootstrap.is_some()
        {
            return Err(io::Error::other(
                "named-pipe rendezvous connection is not ready for bootstrap",
            ));
        }
        let bootstrap = Bootstrap::new(client_token);
        self.set_deadline(Some(bootstrap.deadline))?;
        self.bootstrap = Some(bootstrap);
        if self.start_bootstrap_read(CLIENT_ANNOUNCE_BYTES).is_err() {
            self.abort_bootstrap()?;
        }
        Ok(())
    }

    pub(super) fn promote_bootstrap(&mut self, token: u64) -> io::Result<()> {
        let Some(mut bootstrap) = self.bootstrap.take() else {
            return Err(io::Error::other("completed bootstrap has no return pipe"));
        };
        if bootstrap.token != token
            || bootstrap.phase != BootstrapPhase::WritingReady
            || bootstrap.outbound_offset != DAEMON_READY_BYTES
            || bootstrap
                .return_pipe
                .as_ref()
                .is_none_or(|pipe| pipe.pending_count() != 0)
        {
            self.bootstrap = Some(bootstrap);
            return Err(io::Error::other(
                "return pipe is not ready for application IPC",
            ));
        }
        let pipe = bootstrap
            .return_pipe
            .take()
            .ok_or_else(|| io::Error::other("completed bootstrap has no return pipe"))?;
        if self.clients.insert(token, pipe).is_some() {
            return Err(io::Error::other("named-pipe client token was reused"));
        }
        Ok(())
    }

    fn set_deadline(&self, deadline: Option<Instant>) -> io::Result<()> {
        *self
            .signal
            .deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = deadline;
        if unsafe {
            PostQueuedCompletionStatus(self.port.raw(), 0, TIMER_UPDATE_COMPLETION_KEY, ptr::null())
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn start_bootstrap_read(&mut self, record_bytes: usize) -> io::Result<()> {
        let offset = self
            .bootstrap
            .as_ref()
            .ok_or_else(|| io::Error::other("bootstrap state is missing"))?
            .read_offset;
        let (_, pipe) = self
            .listener
            .as_mut()
            .ok_or_else(|| io::Error::other("bootstrap lost its B pipe"))?;
        if pipe.read.pending || offset >= record_bytes {
            return Err(io::Error::other("bootstrap B read is not ready"));
        }
        pipe.read.pending = true;
        let started = unsafe {
            ReadFile(
                pipe.handle.raw(),
                pipe.read_buffer.as_mut_ptr().add(offset),
                (record_bytes - offset) as u32,
                ptr::null_mut(),
                pipe.read.pointer(),
            )
        };
        if started == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_IO_PENDING {
                pipe.read.pending = false;
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        Ok(())
    }

    fn start_bootstrap_write(&mut self) -> io::Result<()> {
        let (source, remaining) = {
            let bootstrap = self
                .bootstrap
                .as_ref()
                .ok_or_else(|| io::Error::other("bootstrap state is missing"))?;
            if bootstrap.outbound_offset >= bootstrap.outbound_len {
                return Err(io::Error::other("bootstrap R write is not ready"));
            }
            (
                unsafe { bootstrap.outbound.as_ptr().add(bootstrap.outbound_offset) },
                bootstrap.outbound_len - bootstrap.outbound_offset,
            )
        };
        let pipe = self
            .bootstrap
            .as_mut()
            .and_then(|bootstrap| bootstrap.return_pipe.as_mut())
            .ok_or_else(|| io::Error::other("bootstrap lost its R pipe"))?;
        if pipe.write.pending {
            return Err(io::Error::other("bootstrap R write is already pending"));
        }
        pipe.write.pending = true;
        let started = unsafe {
            WriteFile(
                pipe.handle.raw(),
                source,
                remaining as u32,
                ptr::null_mut(),
                pipe.write.pointer(),
            )
        };
        if started == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_IO_PENDING {
                pipe.write.pending = false;
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        Ok(())
    }

    fn open_return_pipe(&self, id: &ReturnPipeId, token: u64) -> io::Result<PipeInstance> {
        let logon_sid = LogonSid::new(&self.identity.logon_sid_name).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid logon SID for bootstrap",
            )
        })?;
        let name = derive_return_pipe_name(&logon_sid, id);
        let wide_name = nul_terminated_wide(Path::new(&name));
        let handle = unsafe {
            CreateFileW(
                wide_name.as_ptr(),
                BOOTSTRAP_ACCESS_MASK,
                0,
                ptr::null(),
                OPEN_EXISTING,
                // Do not request dynamic context tracking: the default is static.
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                ptr::null_mut(),
            )
        };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let handle = Handle(handle);
        let associated =
            unsafe { CreateIoCompletionPort(handle.raw(), self.port.raw(), token as usize, 0) };
        if associated.is_null() || associated != self.port.raw() {
            return Err(io::Error::last_os_error());
        }
        Ok(PipeInstance::new(handle.into_raw()))
    }

    fn abort_bootstrap(&mut self) -> io::Result<()> {
        if self.bootstrap.is_none() {
            return Ok(());
        }
        let needs_cancel = self
            .bootstrap
            .as_ref()
            .is_some_and(|bootstrap| bootstrap.phase != BootstrapPhase::Cancelling);
        if needs_cancel {
            if let Some(bootstrap) = self.bootstrap.as_mut() {
                bootstrap.phase = BootstrapPhase::Cancelling;
            }
            self.set_deadline(None)?;
            if let Some((_, pipe)) = self.listener.as_ref()
                && pipe.pending_count() != 0
            {
                cancel_pipe(pipe.handle.raw())?;
            }
            if let Some(pipe) = self
                .bootstrap
                .as_ref()
                .and_then(|bootstrap| bootstrap.return_pipe.as_ref())
                && pipe.pending_count() != 0
            {
                cancel_pipe(pipe.handle.raw())?;
            }
        }
        if self.bootstrap_quiescent() {
            self.finish_aborted_bootstrap()?;
        }
        Ok(())
    }

    fn bootstrap_quiescent(&self) -> bool {
        self.listener
            .as_ref()
            .is_none_or(|(_, pipe)| pipe.pending_count() == 0)
            && self
                .bootstrap
                .as_ref()
                .and_then(|bootstrap| bootstrap.return_pipe.as_ref())
                .is_none_or(|pipe| pipe.pending_count() == 0)
    }

    fn finish_aborted_bootstrap(&mut self) -> io::Result<()> {
        let Some(bootstrap) = self.bootstrap.as_ref() else {
            return Ok(());
        };
        if bootstrap.phase != BootstrapPhase::Cancelling || !self.bootstrap_quiescent() {
            return Err(io::Error::other(
                "bootstrap cleanup began before I/O drained",
            ));
        }
        drop(self.bootstrap.take());
        let token = self
            .listener
            .as_ref()
            .map(|(token, _)| *token)
            .ok_or_else(|| io::Error::other("bootstrap lost its B pipe"))?;
        self.reset_listener(token)
    }

    fn expire_bootstrap(&mut self) -> io::Result<()> {
        if self.bootstrap.as_ref().is_some_and(|bootstrap| {
            bootstrap.phase != BootstrapPhase::Cancelling && Instant::now() >= bootstrap.deadline
        }) {
            self.abort_bootstrap()?;
        }
        Ok(())
    }

    fn advance_bootstrap(
        &mut self,
        completion: PipeCompletion,
    ) -> io::Result<Option<PipeCompletion>> {
        self.expire_bootstrap()?;
        let Some((token, phase)) = self
            .bootstrap
            .as_ref()
            .map(|bootstrap| (bootstrap.token, bootstrap.phase))
        else {
            return Ok(None);
        };
        if phase == BootstrapPhase::Cancelling {
            if self.bootstrap_quiescent() {
                self.finish_aborted_bootstrap()?;
            }
            return Ok(None);
        }

        match completion {
            PipeCompletion::Read {
                token: completed_token,
                bytes,
                result,
            } if completed_token == self.listener_token().unwrap_or_default() => {
                if result.is_err() || bytes == 0 {
                    self.abort_bootstrap()?;
                    return Ok(None);
                }
                let expected = match phase {
                    BootstrapPhase::ReadingAnnounce => CLIENT_ANNOUNCE_BYTES,
                    BootstrapPhase::ReadingEcho => CLIENT_ECHO_BYTES,
                    _ => {
                        self.abort_bootstrap()?;
                        return Ok(None);
                    }
                };
                let offset = {
                    let bootstrap = self.bootstrap.as_mut().expect("bootstrap was checked");
                    bootstrap.read_offset = bootstrap
                        .read_offset
                        .checked_add(bytes)
                        .filter(|offset| *offset <= expected)
                        .ok_or_else(|| io::Error::other("bootstrap B read exceeded its record"))?;
                    bootstrap.read_offset
                };
                if offset < expected {
                    if self.start_bootstrap_read(expected).is_err() {
                        self.abort_bootstrap()?;
                    }
                    return Ok(None);
                }

                let pipe = self
                    .listener
                    .as_ref()
                    .expect("B pipe was checked")
                    .1
                    .handle
                    .raw();
                if let Err(PeerAuthenticationError::Rejected) =
                    verify_pipe_client(pipe, &self.identity)
                {
                    self.abort_bootstrap()?;
                    return Ok(None);
                }

                if phase == BootstrapPhase::ReadingAnnounce {
                    let parsed = {
                        let (_, pipe) = self.listener.as_ref().expect("B pipe was checked");
                        ClientAnnounce::parse(&pipe.read_buffer[..CLIENT_ANNOUNCE_BYTES])
                    };
                    let announce = match parsed {
                        Ok(announce) => announce,
                        Err(_) => {
                            self.abort_bootstrap()?;
                            return Ok(None);
                        }
                    };
                    let mut challenge = [0u8; 16];
                    let status = unsafe {
                        BCryptGenRandom(
                            ptr::null_mut(),
                            challenge.as_mut_ptr(),
                            challenge.len() as u32,
                            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
                        )
                    };
                    if status != 0 {
                        return Err(io::Error::other(format!(
                            "BCryptGenRandom failed with NTSTATUS {status:#x}"
                        )));
                    }
                    let return_pipe = match self.open_return_pipe(announce.identifier(), token) {
                        Ok(pipe) => pipe,
                        Err(_) => {
                            self.abort_bootstrap()?;
                            return Ok(None);
                        }
                    };
                    let hello = DaemonHello::new(challenge).encode();
                    let bootstrap = self.bootstrap.as_mut().expect("bootstrap was checked");
                    bootstrap.challenge = challenge;
                    bootstrap.phase = BootstrapPhase::WritingHello;
                    bootstrap.return_pipe = Some(return_pipe);
                    bootstrap.outbound[..DAEMON_HELLO_BYTES].copy_from_slice(&hello);
                    bootstrap.outbound_len = DAEMON_HELLO_BYTES;
                    bootstrap.outbound_offset = 0;
                    if self.start_bootstrap_write().is_err() {
                        self.abort_bootstrap()?;
                    }
                    return Ok(None);
                }

                let parsed = {
                    let (_, pipe) = self.listener.as_ref().expect("B pipe was checked");
                    ClientEcho::parse(&pipe.read_buffer[..CLIENT_ECHO_BYTES])
                };
                let echo = match parsed {
                    Ok(echo) => echo,
                    Err(_) => {
                        self.abort_bootstrap()?;
                        return Ok(None);
                    }
                };
                let expected_echo =
                    ClientEcho::new(self.bootstrap.as_ref().unwrap().challenge).encode();
                if echo.encode() != expected_echo {
                    self.abort_bootstrap()?;
                    return Ok(None);
                }
                let ready = DaemonReady::new(READY_STATUS_OK).encode();
                let bootstrap = self.bootstrap.as_mut().expect("bootstrap was checked");
                bootstrap.phase = BootstrapPhase::WritingReady;
                bootstrap.outbound[..DAEMON_READY_BYTES].copy_from_slice(&ready);
                bootstrap.outbound_len = DAEMON_READY_BYTES;
                bootstrap.outbound_offset = 0;
                if self.start_bootstrap_write().is_err() {
                    self.abort_bootstrap()?;
                    return Ok(None);
                }
                Ok(None)
            }
            PipeCompletion::Write {
                token: completed_token,
                bytes,
                result,
            } if completed_token == token => {
                if phase != BootstrapPhase::WritingHello && phase != BootstrapPhase::WritingReady {
                    self.abort_bootstrap()?;
                    return Ok(None);
                }
                if result.is_err() || bytes == 0 {
                    self.abort_bootstrap()?;
                    return Ok(None);
                }
                let finished = {
                    let bootstrap = self.bootstrap.as_mut().expect("bootstrap was checked");
                    bootstrap.outbound_offset = bootstrap
                        .outbound_offset
                        .checked_add(bytes)
                        .filter(|offset| *offset <= bootstrap.outbound_len)
                        .ok_or_else(|| io::Error::other("bootstrap R write exceeded its record"))?;
                    bootstrap.outbound_offset == bootstrap.outbound_len
                };
                if !finished {
                    if self.start_bootstrap_write().is_err() {
                        self.abort_bootstrap()?;
                        return Ok(None);
                    }
                    return Ok(None);
                }
                if phase == BootstrapPhase::WritingHello {
                    let bootstrap = self.bootstrap.as_mut().expect("bootstrap was checked");
                    bootstrap.phase = BootstrapPhase::ReadingEcho;
                    bootstrap.read_offset = 0;
                    if self.start_bootstrap_read(CLIENT_ECHO_BYTES).is_err() {
                        self.abort_bootstrap()?;
                    }
                    return Ok(None);
                }
                self.set_deadline(None)?;
                let listener_token = self
                    .listener_token()
                    .ok_or_else(|| io::Error::other("bootstrap lost its B pipe"))?;
                self.reset_listener(listener_token)?;
                Ok(Some(PipeCompletion::BootstrapReady { token }))
            }
            _ => {
                self.abort_bootstrap()?;
                Ok(None)
            }
        }
    }
    pub(super) fn start_read(&mut self, token: u64) -> io::Result<()> {
        let pipe = self.client_mut(token)?;
        if pipe.read.pending {
            return Ok(());
        }
        pipe.read.pending = true;
        let started = unsafe {
            windows_sys::Win32::Storage::FileSystem::ReadFile(
                pipe.handle.raw(),
                pipe.read_buffer.as_mut_ptr(),
                READ_CHUNK as u32,
                ptr::null_mut(),
                pipe.read.pointer(),
            )
        };
        if started == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_IO_PENDING {
                pipe.read.pending = false;
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        Ok(())
    }

    pub(super) fn start_write(&mut self, token: u64, bytes: &[u8]) -> io::Result<()> {
        let pipe = self.client_mut(token)?;
        if pipe.write.pending || bytes.is_empty() {
            return Ok(());
        }
        let count = bytes.len().min(WRITE_BUDGET).min(u32::MAX as usize) as u32;
        pipe.write.pending = true;
        let started = unsafe {
            windows_sys::Win32::Storage::FileSystem::WriteFile(
                pipe.handle.raw(),
                bytes.as_ptr(),
                count,
                ptr::null_mut(),
                pipe.write.pointer(),
            )
        };
        if started == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_IO_PENDING {
                pipe.write.pending = false;
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        Ok(())
    }
    pub(super) fn next_completion(&mut self) -> io::Result<Option<PipeCompletion>> {
        loop {
            self.expire_bootstrap()?;
            let raw = {
                let mut queue = self
                    .signal
                    .queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let completion = queue.pop_front();
                unsafe {
                    if queue.is_empty() {
                        ResetEvent(self.signal.event.raw());
                    } else {
                        SetEvent(self.signal.event.raw());
                    }
                }
                completion
            };
            let Some(raw) = raw else {
                return Ok(None);
            };
            let completion = self.finish_completion(raw)?;
            let bootstrap_token = self.bootstrap.as_ref().map(|bootstrap| bootstrap.token);
            let internal_bootstrap = match &completion {
                PipeCompletion::Read { token, .. } => {
                    bootstrap_token.is_some() && Some(*token) == self.listener_token()
                }
                PipeCompletion::Write { token, .. } => bootstrap_token == Some(*token),
                _ => false,
            };
            if internal_bootstrap {
                if let Some(ready) = self.advance_bootstrap(completion)? {
                    return Ok(Some(ready));
                }
                continue;
            }
            return Ok(Some(completion));
        }
    }

    fn finish_completion(&mut self, completion: RawCompletion) -> io::Result<PipeCompletion> {
        let token = u64::try_from(completion.token)
            .map_err(|_| io::Error::other("IOCP returned an invalid pipe token"))?;
        let is_listener = self.listener.as_ref().is_some_and(|(id, _)| *id == token);
        let is_return_pipe = self
            .bootstrap
            .as_ref()
            .is_some_and(|bootstrap| bootstrap.token == token);
        let instance = if is_listener {
            &mut self
                .listener
                .as_mut()
                .expect("listener was checked above")
                .1
        } else if is_return_pipe {
            self.bootstrap
                .as_mut()
                .and_then(|bootstrap| bootstrap.return_pipe.as_mut())
                .ok_or_else(|| io::Error::other("IOCP completion has no bootstrap return pipe"))?
        } else {
            self.clients
                .get_mut(&token)
                .ok_or_else(|| io::Error::other("IOCP completion belongs to an unknown pipe"))?
        };
        let pointer = completion.overlapped;
        let kind = if instance.connect.pointer() as usize == pointer {
            if !instance.connect.pending {
                return Err(io::Error::other("duplicate named-pipe connect completion"));
            }
            instance.connect.pending = false;
            instance.connect.reset();
            0
        } else if instance.read.pointer() as usize == pointer {
            if !instance.read.pending {
                return Err(io::Error::other("duplicate named-pipe read completion"));
            }
            instance.read.pending = false;
            instance.read.reset();
            1
        } else if instance.write.pointer() as usize == pointer {
            if !instance.write.pending {
                return Err(io::Error::other("duplicate named-pipe write completion"));
            }
            instance.write.pending = false;
            instance.write.reset();
            2
        } else {
            return Err(io::Error::other(
                "IOCP returned an unknown OVERLAPPED operation",
            ));
        };
        let result = if completion.error == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(completion.error as i32))
        };
        match kind {
            0 => Ok(PipeCompletion::Connected { token, result }),
            1 => Ok(PipeCompletion::Read {
                token,
                bytes: completion.bytes as usize,
                result,
            }),
            _ => Ok(PipeCompletion::Write {
                token,
                bytes: completion.bytes as usize,
                result,
            }),
        }
    }

    pub(super) fn read_data(&self, token: u64, bytes: usize) -> io::Result<&[u8]> {
        let pipe = self
            .clients
            .get(&token)
            .ok_or_else(|| io::Error::other("read completion belongs to an unknown pipe"))?;
        if bytes > READ_CHUNK {
            return Err(io::Error::other("named-pipe read exceeded its buffer"));
        }
        Ok(&pipe.read_buffer[..bytes])
    }

    pub(super) fn cancel_client(&mut self, token: u64) -> io::Result<()> {
        let pipe = self.client_mut(token)?;
        if unsafe { CancelIoEx(pipe.handle.raw(), ptr::null()) } == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_NOT_FOUND {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        Ok(())
    }
    pub(super) fn cancel_read(&mut self, token: u64) -> io::Result<()> {
        let pipe = self.client_mut(token)?;
        if !pipe.read.pending {
            return Ok(());
        }
        if unsafe { CancelIoEx(pipe.handle.raw(), pipe.read.pointer()) } == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_NOT_FOUND {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        Ok(())
    }

    pub(super) fn wake(&self) -> io::Result<()> {
        if unsafe { SetEvent(self.signal.event.raw()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn client_quiescent(&self, token: u64) -> bool {
        self.clients
            .get(&token)
            .is_none_or(|pipe| pipe.pending_count() == 0)
    }

    pub(super) fn remove_client(&mut self, token: u64) -> bool {
        if self.client_quiescent(token) {
            self.clients.remove(&token).is_some()
        } else {
            false
        }
    }

    fn client_mut(&mut self, token: u64) -> io::Result<&mut PipeInstance> {
        self.clients
            .get_mut(&token)
            .ok_or_else(|| io::Error::other("named-pipe client does not exist"))
    }
}
impl Drop for WindowsPipeServer {
    fn drop(&mut self) {
        if let Some((_, listener)) = self.listener.as_ref() {
            unsafe { CancelIoEx(listener.handle.raw(), ptr::null()) };
        }
        if let Some(pipe) = self
            .bootstrap
            .as_ref()
            .and_then(|bootstrap| bootstrap.return_pipe.as_ref())
        {
            unsafe { CancelIoEx(pipe.handle.raw(), ptr::null()) };
        }
        for pipe in self.clients.values() {
            unsafe { CancelIoEx(pipe.handle.raw(), ptr::null()) };
        }
        while self.pending_count() != 0 {
            unsafe { WaitForSingleObject(self.signal.event.raw(), INFINITE) };
            let mut completions = VecDeque::new();
            {
                let mut queue = self
                    .signal
                    .queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                std::mem::swap(&mut completions, &mut *queue);
                unsafe { ResetEvent(self.signal.event.raw()) };
            }
            while let Some(completion) = completions.pop_front() {
                let _ = self.finish_completion(completion);
            }
        }
        unsafe {
            PostQueuedCompletionStatus(self.port.raw(), 0, SHUTDOWN_COMPLETION_KEY, ptr::null());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl WindowsPipeServer {
    fn pending_count(&self) -> usize {
        self.listener
            .as_ref()
            .map_or(0, |(_, pipe)| pipe.pending_count())
            + self
                .bootstrap
                .as_ref()
                .and_then(|bootstrap| bootstrap.return_pipe.as_ref())
                .map_or(0, PipeInstance::pending_count)
            + self
                .clients
                .values()
                .map(PipeInstance::pending_count)
                .sum::<usize>()
    }
}

fn completion_worker(port: HANDLE, signal: Arc<CompletionSignal>) {
    loop {
        let deadline = *signal
            .deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let timeout = deadline
            .map(|deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_micros()
                    .div_ceil(1_000)
                    .min(u128::from(u32::MAX - 1)) as u32
            })
            .unwrap_or(INFINITE);
        let mut bytes = 0;
        let mut token = 0;
        let mut overlapped = ptr::null_mut();
        let succeeded = unsafe {
            GetQueuedCompletionStatus(port, &mut bytes, &mut token, &mut overlapped, timeout)
        };
        if overlapped.is_null() && token == SHUTDOWN_COMPLETION_KEY {
            break;
        }
        if overlapped.is_null() && token == TIMER_UPDATE_COMPLETION_KEY {
            continue;
        }
        if overlapped.is_null() {
            if succeeded == 0 && unsafe { GetLastError() } == WAIT_TIMEOUT {
                let expired = {
                    let mut deadline = signal
                        .deadline
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        *deadline = None;
                        true
                    } else {
                        false
                    }
                };
                if expired {
                    unsafe { SetEvent(signal.event.raw()) };
                }
            }
            continue;
        }
        let error = if succeeded == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        let mut queue = signal
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.push_back(RawCompletion {
            token,
            overlapped: overlapped as usize,
            bytes,
            error,
        });
        unsafe { SetEvent(signal.event.raw()) };
    }
}

fn cancel_pipe(handle: HANDLE) -> io::Result<()> {
    if unsafe { CancelIoEx(handle, ptr::null()) } == 0 {
        let error = unsafe { GetLastError() };
        if error != ERROR_NOT_FOUND {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
    }
    Ok(())
}
struct TokenSid {
    storage: Vec<usize>,
    offset: usize,
}

impl TokenSid {
    fn as_ptr(&self) -> windows_sys::Win32::Security::PSID {
        unsafe {
            self.storage
                .as_ptr()
                .cast::<u8>()
                .add(self.offset)
                .cast_mut()
                .cast()
        }
    }
}

struct CurrentIdentity {
    user_sid: TokenSid,
    logon_sid: TokenSid,
    logon_sid_name: String,
}

impl CurrentIdentity {
    fn open() -> io::Result<Self> {
        let mut raw_token = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Handle(raw_token);
        let user_sid = token_sid(token.raw(), TokenUser)?;
        let logon_sid = token_sid(token.raw(), TokenLogonSid)?;
        let logon_sid_name = sid_to_string(logon_sid.as_ptr())?;
        if !is_logon_sid_name(&logon_sid_name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process token does not contain a valid logon SID",
            ));
        }
        Ok(Self {
            user_sid,
            logon_sid,
            logon_sid_name,
        })
    }
}

enum PeerAuthenticationError {
    Rejected,
}

fn verify_pipe_client(
    pipe: HANDLE,
    expected: &CurrentIdentity,
) -> Result<(), PeerAuthenticationError> {
    if unsafe { ImpersonateNamedPipeClient(pipe) } == 0 {
        return Err(PeerAuthenticationError::Rejected);
    }
    let result = (|| {
        let mut raw_token = ptr::null_mut();
        if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut raw_token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Handle(raw_token);
        let user_sid = token_sid(token.raw(), TokenUser)?;
        let logon_sid = token_sid(token.raw(), TokenLogonSid)?;
        if unsafe {
            EqualSid(user_sid.as_ptr(), expected.user_sid.as_ptr()) == 0
                || EqualSid(logon_sid.as_ptr(), expected.logon_sid.as_ptr()) == 0
        } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "named-pipe writer token does not match this logon identity",
            ));
        }
        Ok(())
    })();
    if unsafe { RevertToSelf() } == 0 {
        std::process::abort();
    }
    result.map_err(|_| PeerAuthenticationError::Rejected)
}
struct PipeSecurity {
    descriptor: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
    attributes: SECURITY_ATTRIBUTES,
}

impl PipeSecurity {
    fn new(identity: &CurrentIdentity) -> io::Result<Self> {
        let user_sid = sid_to_string(identity.user_sid.as_ptr())?;
        // OWNER RIGHTS prevents another logon of the same user from changing this ACL.
        let sddl = format!(
            "O:{user_sid}D:P(D;;WDWO;;;OW)(A;;0x{BOOTSTRAP_ACCESS_MASK:08x};;;{})",
            identity.logon_sid_name
        );
        let wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                1,
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
                "Windows returned a null pipe security descriptor",
            ));
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.cast(),
            bInheritHandle: 0,
        };
        Ok(Self {
            descriptor,
            attributes,
        })
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            unsafe { LocalFree(self.descriptor.cast::<c_void>()) };
        }
    }
}

fn token_sid(
    token: HANDLE,
    class: windows_sys::Win32::Security::TOKEN_INFORMATION_CLASS,
) -> io::Result<TokenSid> {
    let mut required = 0;
    let queried = unsafe { GetTokenInformation(token, class, ptr::null_mut(), 0, &mut required) };
    if queried != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned token data for a zero-sized query",
        ));
    }
    let error = unsafe { GetLastError() };
    if error != ERROR_INSUFFICIENT_BUFFER || required == 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let words = (required as usize).div_ceil(size_of::<usize>());
    let mut storage = vec![0usize; words];
    let buffer_bytes = words
        .checked_mul(size_of::<usize>())
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "token data is too large"))?;
    if unsafe {
        GetTokenInformation(
            token,
            class,
            storage.as_mut_ptr().cast(),
            buffer_bytes,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let sid = unsafe {
        if class == TokenUser {
            (*storage.as_ptr().cast::<TOKEN_USER>()).User.Sid
        } else if class == TokenLogonSid {
            let groups = &*storage.as_ptr().cast::<TOKEN_GROUPS>();
            if groups.GroupCount != 1
                || groups.Groups[0].Attributes & LOGON_ID_GROUP_ATTRIBUTES
                    != LOGON_ID_GROUP_ATTRIBUTES
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "process token did not return exactly one logon SID",
                ));
            }
            if groups.Groups[0].Attributes & SE_GROUP_ENABLED == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "process token logon SID is not enabled for pipe access",
                ));
            }
            groups.Groups[0].Sid
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported token SID information class",
            ));
        }
    };
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process token returned an invalid SID",
        ));
    }
    let base = storage.as_ptr() as usize;
    let offset = (sid as usize)
        .checked_sub(base)
        .filter(|offset| *offset < storage.len() * size_of::<usize>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "token SID is out of bounds"))?;
    Ok(TokenSid { storage, offset })
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0.cast::<c_void>()) };
        }
    }
}
fn sid_to_string(sid: windows_sys::Win32::Security::PSID) -> io::Result<String> {
    let mut wide = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut wide) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if wide.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a null SID string",
        ));
    }
    let _allocation = LocalAllocation(wide.cast());
    let mut length = 0usize;
    unsafe {
        while *wide.add(length) != 0 {
            length += 1;
        }
    }
    let text = unsafe { slice::from_raw_parts(wide, length) };
    String::from_utf16(text).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned invalid SID text",
        )
    })
}

fn is_logon_sid_name(name: &str) -> bool {
    let mut parts = name.split('-');
    parts.next() == Some("S")
        && parts.next() == Some("1")
        && parts.next() == Some("5")
        && parts.next() == Some("5")
        && parts.next().is_some_and(|part| part.parse::<u32>().is_ok())
        && parts.next().is_some_and(|part| part.parse::<u32>().is_ok())
        && parts.next().is_none()
}

fn endpoint_path(logon_sid: &str) -> PathBuf {
    PathBuf::from(format!("{ENDPOINT_PREFIX}{logon_sid}{ENDPOINT_SUFFIX}"))
}

fn nul_terminated_wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
