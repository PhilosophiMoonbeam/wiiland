//! Nonblocking IPC transport; runtime commands are completed by the reactor.
mod connection;
mod socket;
use connection::*;
use socket::*;
use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use wiiland_ipc::{
    Command, DeviceInfo, Notification, ProtocolError, ProtocolErrorCode, ResponseResult,
    ServerMessage, Status, encode_frame,
};

const LISTENER_TOKEN: u64 = 1;
const MAX_CLIENTS: usize = 64;
const MAX_QUEUED_BYTES: usize = 256 * 1024;
const ACCEPT_BUDGET: usize = 8;
const READ_BUDGET: usize = 8;
const READ_CHUNK: usize = 16 * 1024;
const FRAME_BUDGET: usize = 8;
const WRITE_BUDGET: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct PollSource {
    pub token: u64,
    pub fd: RawFd,
    pub events: i16,
}

/// Runtime-owned status and device snapshots are supplied to [`Self::handle_ready`]
/// and are never retained by this transport.
pub(crate) struct IpcServer {
    listener: UnixListener,
    path: PathBuf,
    parent: PinnedParent,
    _lock: SocketLock,
    inode: (u64, u64),
    clients: HashMap<u64, Client>,
    next_token: u64,
    #[cfg(test)]
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
        Self::bind_with_setup(path, |_, _, _| Ok(()))
    }

    fn bind_with_setup<F>(path: impl AsRef<Path>, setup_hook: F) -> io::Result<Self>
    where
        F: FnOnce(&Path, &UnixListener, (u64, u64)) -> io::Result<()>,
    {
        Self::bind_with_hooks(path, setup_hook, || {})
    }

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
            #[cfg(test)]
            before_drop_cleanup: None,
        })
    }

    #[cfg(test)]
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
            if token > LISTENER_TOKEN && !self.clients.contains_key(&token) {
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
                        return;
                    }
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
                Err(error) => {
                    client.queue(&ServerMessage::Error {
                        id: Some(id),
                        error,
                    });
                }
            }
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
                continue;
            }
            client.queued_bytes += frame.len();
            client.output.push_back(Pending {
                bytes: frame.clone(),
                offset: 0,
            });
        }
        self.clients.retain(|_, client| !client.immediate_close);
    }
}

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
#[cfg(test)]
mod tests;
