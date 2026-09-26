//! Nonblocking IPC transport; runtime commands are completed by the reactor.
mod connection;
#[cfg(unix)]
mod socket;
#[cfg(windows)]
mod windows_pipe;
use connection::*;
#[cfg(unix)]
use socket::*;
use std::collections::HashMap;
use std::io;
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use windows_pipe::*;
#[cfg(windows)]
use windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED;
#[cfg(windows)]
use windows_sys::Win32::Foundation::HANDLE;

use wiiland_ipc::{
    Command, DeviceInfo, Notification, ProtocolError, ProtocolErrorCode, ResponseResult,
    ServerMessage, Status, encode_frame,
};

const LISTENER_TOKEN: u64 = 1;
const MAX_CLIENTS: usize = 64;
const MAX_QUEUED_BYTES: usize = 256 * 1024;
#[cfg(unix)]
const ACCEPT_BUDGET: usize = 8;
const READ_BUDGET: usize = 8;
const READ_CHUNK: usize = 16 * 1024;
const FRAME_BUDGET: usize = 8;
const WRITE_BUDGET: usize = 64 * 1024;
#[cfg(windows)]
const COMPLETION_BUDGET: usize = 64;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct PollSource {
    pub token: u64,
    #[cfg(unix)]
    pub fd: RawFd,
    #[cfg(unix)]
    pub events: i16,
    #[cfg(windows)]
    pub handle: HANDLE,
}

/// Runtime-owned status and device snapshots are supplied to [`Self::handle_ready`]
/// and are never retained by this transport.
pub(crate) struct IpcServer {
    #[cfg(unix)]
    listener: UnixListener,
    path: PathBuf,
    #[cfg(unix)]
    parent: PinnedParent,
    #[cfg(unix)]
    _lock: SocketLock,
    #[cfg(unix)]
    inode: (u64, u64),
    #[cfg(windows)]
    transport: WindowsPipeServer,
    clients: HashMap<u64, Client>,
    #[cfg(windows)]
    continuations: std::collections::VecDeque<u64>,
    next_token: u64,
    #[cfg(all(test, unix))]
    before_drop_cleanup: Option<Box<dyn FnMut() + Send>>,
}

impl std::fmt::Debug for IpcServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcServer")
            .field("path", &self.path)
            .field("clients", &self.clients.len())
            .finish()
    }
}

impl IpcServer {
    pub(crate) fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Self::bind_unix(path)
        }
        #[cfg(windows)]
        {
            let path = path.as_ref().to_path_buf();
            let listener_token = 2;
            let transport = WindowsPipeServer::bind(&path, listener_token)?;
            Ok(Self {
                path,
                transport,
                clients: HashMap::new(),
                continuations: std::collections::VecDeque::new(),
                next_token: listener_token + 1,
            })
        }
    }

    #[cfg(unix)]
    fn bind_unix(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::bind_with_setup(path, |_, _, _| Ok(()))
    }

    #[cfg(unix)]
    fn bind_with_setup<F>(path: impl AsRef<Path>, setup_hook: F) -> io::Result<Self>
    where
        F: FnOnce(&Path, &UnixListener, (u64, u64)) -> io::Result<()>,
    {
        Self::bind_with_hooks(path, setup_hook, || {})
    }

    #[cfg(unix)]
    fn bind_with_hooks<F, S>(
        path: impl AsRef<Path>,
        setup_hook: F,
        mut before_stale_capture: S,
    ) -> io::Result<Self>
    where
        F: FnOnce(&Path, &UnixListener, (u64, u64)) -> io::Result<()>,
        S: FnMut(),
    {
        let path = path.as_ref().to_path_buf();
        let name = socket_name(&path)?.to_os_string();
        let parent_path = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf();
        ensure_private_parent(&parent_path)?;
        let parent = PinnedParent::open(parent_path)?;

        let lock = acquire_socket_lock(&parent, &name)?;

        match parent.entry_metadata(&name) {
            Ok(meta) => {
                if !meta.is_socket() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "IPC path is not a socket",
                    ));
                }
                ensure_entry_owner(meta, "IPC stale socket")?;
                match UnixStream::connect(parent.entry_path(&name)) {
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            "IPC socket is already in use",
                        ));
                    }
                    Err(error) if is_stale_connect_error(&error) => {
                        before_stale_capture();
                        remove_expected_entry(&parent, &name, meta.inode)?;
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let listener = UnixListener::bind(parent.entry_path(&name))?;
        let inode = match parent.entry_metadata(&name) {
            Ok(meta) if meta.is_socket() => meta.inode,
            Ok(_) => {
                drop(listener);
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "IPC socket path changed during bind",
                ));
            }
            Err(error) => {
                drop(listener);
                return Err(error);
            }
        };
        let mut guard = BoundSocketGuard::new(&parent, &name, inode);
        setup_hook(&path, &listener, inode)?;
        setup_bound_socket(&parent, &name, &listener, inode)?;
        parent.verify_public_identity()?;
        guard.disarm();
        drop(guard);

        Ok(Self {
            listener,
            path,
            parent,
            inode,
            _lock: lock,
            clients: HashMap::new(),
            next_token: 2,
            #[cfg(all(test, unix))]
            before_drop_cleanup: None,
        })
    }

    #[cfg(all(test, unix))]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn has_input_subscribers(&self) -> bool {
        self.clients.values().any(|client| {
            client.negotiated && client.input && !client.closing && !client.immediate_close
        })
    }

    pub(crate) fn poll_sources(&self, sources: &mut Vec<PollSource>) {
        sources.clear();
        #[cfg(unix)]
        {
            sources.push(PollSource {
                token: LISTENER_TOKEN,
                fd: self.listener.as_raw_fd(),
                events: libc::POLLIN,
            });
            for (&token, client) in &self.clients {
                let mut events = 0;
                if !client.closing && !client.immediate_close {
                    events |= libc::POLLIN;
                }
                if !client.immediate_close
                    && (!client.output.is_empty() || !client.pending_frames.is_empty())
                {
                    events |= libc::POLLOUT;
                }
                sources.push(PollSource {
                    token,
                    fd: client.stream.as_raw_fd(),
                    events,
                });
            }
        }
        #[cfg(windows)]
        sources.push(PollSource {
            token: LISTENER_TOKEN,
            handle: self.transport.completion_handle(),
        });
    }

    #[cfg(unix)]
    pub(crate) fn handle_ready(
        &mut self,
        token: u64,
        revents: i16,
        status: &mut dyn FnMut(&Path) -> Status,
        devices: &mut dyn FnMut() -> Vec<DeviceInfo>,
    ) -> io::Result<()> {
        if token == LISTENER_TOKEN {
            if revents & libc::POLLNVAL != 0 {
                return Err(io::Error::other("IPC listener became invalid"));
            }
            if revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
                self.accept_ready()?;
            }
            return Ok(());
        }
        if !self.clients.contains_key(&token) {
            return Ok(());
        }
        let socket_path = self.path.as_path();
        let mut remove = false;
        if let Some(client) = self.clients.get_mut(&token) {
            if !client.closing
                && !client.immediate_close
                && (revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
                    || !client.pending_frames.is_empty())
            {
                remove = !read_client(client, socket_path, status, devices);
            }
            if client.immediate_close {
                remove = true;
            }
            if !remove && (revents & libc::POLLOUT != 0 || !client.output.is_empty()) {
                remove = !client.write_ready();
            }
            if client.immediate_close || (client.closing && client.output.is_empty()) {
                remove = true;
            }
        }
        if remove {
            self.clients.remove(&token);
        }
        Ok(())
    }

    #[cfg(windows)]
    pub(crate) fn handle_ready(
        &mut self,
        token: u64,
        status: &mut dyn FnMut(&Path) -> Status,
        devices: &mut dyn FnMut() -> Vec<DeviceInfo>,
    ) -> io::Result<()> {
        if token != LISTENER_TOKEN {
            return Ok(());
        }
        for _ in 0..COMPLETION_BUDGET {
            let Some(completion) = self.transport.next_completion()? else {
                break;
            };
            match completion {
                PipeCompletion::Connected { token, result } => {
                    if result.is_err() || self.clients.len() >= MAX_CLIENTS {
                        self.transport.reject_listener(token)?;
                        continue;
                    }
                    let client_token = self.allocate_token();
                    self.transport.begin_bootstrap(token, client_token)?;
                }
                PipeCompletion::BootstrapReady { token } => {
                    self.transport.promote_bootstrap(token)?;
                    self.clients.insert(token, Client::new());
                    if self.transport.start_read(token).is_err() {
                        self.close_windows_client(token)?;
                    }
                }
                PipeCompletion::Read {
                    token,
                    bytes,
                    result,
                } => {
                    if result.as_ref().is_err_and(|error| {
                        error.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32)
                    }) {
                        self.sync_windows_client(token)?;
                        continue;
                    }
                    if result.is_err() || bytes == 0 {
                        self.close_windows_client(token)?;
                        continue;
                    }
                    if self
                        .clients
                        .get(&token)
                        .is_some_and(|client| client.closing || client.immediate_close)
                    {
                        self.sync_windows_client(token)?;
                        continue;
                    }
                    let input = self.transport.read_data(token, bytes)?;
                    let Some(client) = self.clients.get_mut(&token) else {
                        self.close_windows_client(token)?;
                        continue;
                    };
                    let (alive, has_pending) = {
                        let alive = read_bytes(client, input, &self.path, status, devices);
                        (alive, !client.pending_frames.is_empty())
                    };
                    if !alive {
                        self.close_windows_client(token)?;
                        continue;
                    }
                    if has_pending {
                        self.defer_windows_client(token);
                    }
                    self.sync_windows_client(token)?;
                }
                PipeCompletion::Write {
                    token,
                    bytes,
                    result,
                } => {
                    if result.is_err() || bytes == 0 {
                        self.close_windows_client(token)?;
                        continue;
                    }
                    let written = self
                        .clients
                        .get_mut(&token)
                        .is_some_and(|client| client.consume_written(bytes));
                    if !written {
                        self.close_windows_client(token)?;
                        continue;
                    }
                    self.sync_windows_client(token)?;
                }
            }
        }
        self.process_windows_continuations(status, devices)?;
        self.cleanup_windows_clients();
        self.ensure_windows_listener()
    }

    #[cfg(windows)]
    fn process_windows_continuations(
        &mut self,
        status: &mut dyn FnMut(&Path) -> Status,
        devices: &mut dyn FnMut() -> Vec<DeviceInfo>,
    ) -> io::Result<()> {
        for _ in 0..READ_BUDGET {
            let Some(token) = self.continuations.pop_front() else {
                break;
            };
            let mut keep = false;
            if let Some(client) = self.clients.get_mut(&token) {
                client.continuation_queued = false;
                let mut budget = FRAME_BUDGET;
                if !client.closing
                    && !client.immediate_close
                    && !process_pending(client, &self.path, status, devices, &mut budget)
                {
                    client.immediate_close = true;
                }
                keep =
                    !client.pending_frames.is_empty() && !client.closing && !client.immediate_close;
            }
            if keep {
                self.defer_windows_client(token);
            }
            self.sync_windows_client(token)?;
        }
        if !self.continuations.is_empty() {
            self.transport.wake()?;
        }
        Ok(())
    }

    #[cfg(windows)]
    fn defer_windows_client(&mut self, token: u64) {
        if let Some(client) = self.clients.get_mut(&token)
            && !client.continuation_queued
        {
            client.continuation_queued = true;
            self.continuations.push_back(token);
        }
    }

    #[cfg(windows)]
    fn sync_windows_client(&mut self, token: u64) -> io::Result<()> {
        let Some(client) = self.clients.get(&token) else {
            return Ok(());
        };
        let immediate_close = client.immediate_close;
        let closing = client.closing;
        let has_output = !client.output.is_empty();
        let has_pending_frames = !client.pending_frames.is_empty();
        if immediate_close || (closing && !has_output) {
            self.transport.cancel_client(token)?;
            return Ok(());
        }
        if closing {
            self.transport.cancel_read(token)?;
        }
        if has_output {
            if let Some(bytes) = self
                .clients
                .get(&token)
                .and_then(|client| client.next_write(WRITE_BUDGET))
                && self.transport.start_write(token, bytes).is_err()
            {
                self.close_windows_client(token)?;
            }
        } else if !closing && !has_pending_frames && self.transport.start_read(token).is_err() {
            self.close_windows_client(token)?;
        }
        Ok(())
    }

    #[cfg(windows)]
    fn close_windows_client(&mut self, token: u64) -> io::Result<()> {
        if let Some(client) = self.clients.get_mut(&token) {
            client.immediate_close = true;
            self.transport.cancel_client(token)?;
        }
        self.cleanup_windows_clients();
        Ok(())
    }

    #[cfg(windows)]
    fn cleanup_windows_clients(&mut self) {
        let removable: Vec<u64> = self
            .clients
            .iter()
            .filter_map(|(&token, client)| {
                (client.immediate_close || (client.closing && client.output.is_empty()))
                    .then_some(token)
            })
            .filter(|token| self.transport.client_quiescent(*token))
            .collect();
        for token in removable {
            self.clients.remove(&token);
            self.transport.remove_client(token);
        }
    }

    #[cfg(windows)]
    fn ensure_windows_listener(&mut self) -> io::Result<()> {
        if self.clients.len() < MAX_CLIENTS && self.transport.listener_token().is_none() {
            let token = self.allocate_token();
            self.transport.listen(token)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn accept_ready(&mut self) -> io::Result<()> {
        for _ in 0..ACCEPT_BUDGET {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    if self.clients.len() >= MAX_CLIENTS {
                        continue;
                    }
                    let token = self.allocate_token();
                    self.clients.insert(token, Client::new(stream));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn allocate_token(&mut self) -> u64 {
        loop {
            let token = self.next_token;
            self.next_token = self.next_token.wrapping_add(1);
            if self.next_token == 0 || self.next_token == LISTENER_TOKEN {
                self.next_token = 2;
            }
            #[cfg(windows)]
            let listener_active = self.transport.listener_token() == Some(token);
            #[cfg(unix)]
            let listener_active = false;
            #[cfg(windows)]
            let completion_key_available =
                usize::try_from(token).is_ok_and(|key| key != usize::MAX);
            #[cfg(unix)]
            let completion_key_available = true;
            if token > LISTENER_TOKEN
                && !self.clients.contains_key(&token)
                && !listener_active
                && completion_key_available
            {
                return token;
            }
        }
    }

    pub(crate) fn take_commands(&mut self) -> Vec<(u64, u64, Command)> {
        let mut commands = Vec::new();
        for (&token, client) in &mut self.clients {
            if !client.closing && !client.immediate_close {
                commands.extend(
                    client
                        .commands
                        .drain(..)
                        .map(|(id, command)| (token, id, command)),
                );
            }
        }
        commands
    }

    pub(crate) fn complete_command(
        &mut self,
        token: u64,
        id: u64,
        result: Result<ResponseResult, ProtocolError>,
    ) {
        if let Some(client) = self.clients.get_mut(&token) {
            match result {
                Ok(result) => {
                    if matches!(&result, ResponseResult::CaptureStarted(info) if client.capture.len() >= wiiland_ipc::MAX_CAPTURE_DEVICES && !client.capture.contains(&info.syspath))
                    {
                        client.queue(&ServerMessage::Error {
                            id: Some(id),
                            error: protocol_error(
                                ProtocolErrorCode::InvalidRequest,
                                "capture device limit reached",
                            ),
                        });
                    } else {
                        match &result {
                            ResponseResult::CaptureStarted(info) => {
                                if !client.capture.contains(&info.syspath) {
                                    client.capture.push(info.syspath.clone());
                                }
                            }
                            ResponseResult::CaptureStopped => client.capture.clear(),
                            _ => {}
                        }
                        client.queue_response(id, result);
                    }
                }
                Err(error) => {
                    client.queue(&ServerMessage::Error {
                        id: Some(id),
                        error,
                    });
                }
            }
        }
        #[cfg(windows)]
        {
            let _ = self.sync_windows_client(token);
            self.cleanup_windows_clients();
        }
    }
    pub(crate) fn capture_paths(&self) -> Vec<String> {
        self.clients
            .values()
            .filter(|client| !client.closing && !client.immediate_close)
            .flat_map(|client| client.capture.clone())
            .collect()
    }

    pub(crate) fn publish(&mut self, notification: Notification) {
        let Ok(frame) = encode_frame(&ServerMessage::Notification(notification.clone())) else {
            return;
        };
        let tokens: Vec<u64> = self
            .clients
            .iter()
            .filter_map(|(&token, client)| {
                (client.negotiated
                    && !client.closing
                    && !client.immediate_close
                    && client.wants(&notification))
                .then_some(token)
            })
            .collect();
        for token in tokens {
            let Some(client) = self.clients.get_mut(&token) else {
                continue;
            };
            if client.queued_bytes.saturating_add(frame.len()) > MAX_QUEUED_BYTES {
                client.immediate_close = true;
            } else {
                client.queued_bytes += frame.len();
                client.output.push_back(Pending {
                    bytes: frame.clone(),
                    offset: 0,
                });
            }
            #[cfg(windows)]
            let _ = self.sync_windows_client(token);
        }
        #[cfg(unix)]
        self.clients.retain(|_, client| !client.immediate_close);
        #[cfg(windows)]
        self.cleanup_windows_clients();
    }
}
#[cfg(unix)]
impl Drop for IpcServer {
    fn drop(&mut self) {
        #[cfg(test)]
        if let Some(hook) = self.before_drop_cleanup.as_mut() {
            hook();
        }
        if let Some(name) = self.path.file_name() {
            let _ = remove_expected_entry(&self.parent, name, self.inode);
        }
    }
}
#[cfg(all(test, unix))]
mod tests;
