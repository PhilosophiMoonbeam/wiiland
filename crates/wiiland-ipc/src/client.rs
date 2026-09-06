use std::collections::VecDeque;
use std::env;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{
    Command, DeviceInfo, FrameBuffer, FrameError, Notification, PROTOCOL_MAJOR, ProtocolError,
    Request, ResponseResult, ServerMessage, Status, Subscription, decode_frame, encode_frame,
};

const MAX_NOTIFICATION_BACKLOG_BYTES: usize = 256 * 1024;

/// Errors returned by the blocking IPC client.
#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    Frame(FrameError),
    Server {
        error: ProtocolError,
    },
    UnexpectedResponseId {
        expected: u64,
        actual: u64,
    },
    UnexpectedMessage,
    UnsupportedVersion {
        major: u16,
        minor: u16,
    },
    NotificationBacklogExceeded {
        limit: usize,
    },
    SessionBacklogExceeded {
        limit: usize,
    },
    UnsupportedFeature {
        peer_minor: u16,
        required_minor: u16,
    },
    PrematureEof,
    RuntimeDirectoryMissing,
    RuntimeDirectoryRelative(PathBuf),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "IPC I/O error: {error}"),
            Self::Frame(error) => write!(f, "IPC framing error: {error}"),
            Self::Server { error } => write!(f, "IPC server error: {error}"),
            Self::UnexpectedResponseId { expected, actual } => {
                write!(
                    f,
                    "IPC response id mismatch: expected {expected}, got {actual}"
                )
            }
            Self::UnexpectedMessage => f.write_str("unexpected IPC message"),
            Self::UnsupportedVersion { major, minor } => {
                write!(f, "unsupported IPC protocol version {major}.{minor}")
            }
            Self::NotificationBacklogExceeded { limit } => {
                write!(
                    f,
                    "IPC notification backlog exceeded the {limit}-byte limit"
                )
            }
            Self::SessionBacklogExceeded { limit } => write!(
                f,
                "IPC session exceeded its {limit}-event queue; reconnect to resume"
            ),
            Self::UnsupportedFeature {
                peer_minor,
                required_minor,
            } => write!(
                f,
                "operation requires daemon protocol 1.{required_minor}; connected daemon provides 1.{peer_minor}"
            ),
            Self::PrematureEof => f.write_str("IPC peer closed the connection"),
            Self::RuntimeDirectoryMissing => f.write_str("XDG_RUNTIME_DIR is not set"),
            Self::RuntimeDirectoryRelative(path) => {
                write!(f, "XDG_RUNTIME_DIR is not absolute: {}", path.display())
            }
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<FrameError> for ClientError {
    fn from(error: FrameError) -> Self {
        Self::Frame(error)
    }
}

/// A blocking client for the wiiland daemon's Unix socket protocol.
pub struct Client {
    stream: UnixStream,
    socket_path: PathBuf,
    frames: FrameBuffer,
    notifications: VecDeque<(Notification, usize)>,
    notification_bytes: usize,
    messages: VecDeque<(ServerMessage, usize)>,
    terminated: bool,
    next_id: u64,
    protocol_minor: u16,
}

impl Client {
    /// Connect to `path` and negotiate the protocol version.
    pub fn connect(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let path = path.as_ref().to_path_buf();
        let stream = UnixStream::connect(&path)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut client = Self {
            stream,
            socket_path: path,
            frames: FrameBuffer::new(),
            notifications: VecDeque::new(),
            notification_bytes: 0,
            messages: VecDeque::new(),
            terminated: false,
            next_id: 1,
            protocol_minor: 0,
        };
        client.hello()?;
        client.set_read_timeout(None)?;
        client.set_write_timeout(None)?;
        Ok(client)
    }

    /// Connect using `$XDG_RUNTIME_DIR/wiiland/wiilandd.sock`.
    pub fn connect_default() -> Result<Self, ClientError> {
        Self::connect(default_socket_path()?)
    }

    /// Return the default daemon socket path.
    pub fn default_socket_path() -> Result<PathBuf, ClientError> {
        default_socket_path()
    }

    /// The path used to establish this connection.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Set the read timeout for subsequent socket reads.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), ClientError> {
        self.stream
            .set_read_timeout(timeout)
            .map_err(ClientError::Io)
    }

    /// Set the write timeout for subsequent socket writes.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> Result<(), ClientError> {
        self.stream
            .set_write_timeout(timeout)
            .map_err(ClientError::Io)
    }

    /// Request the daemon status snapshot.
    pub fn status(&mut self) -> Result<Status, ClientError> {
        match self.request(Command::Status)? {
            ResponseResult::Status(status) => Ok(status),
            _ => Err(ClientError::UnexpectedMessage),
        }
    }

    /// Request the currently known devices.
    pub fn devices(&mut self) -> Result<Vec<DeviceInfo>, ClientError> {
        match self.request(Command::Devices)? {
            ResponseResult::Devices(devices) => Ok(devices),
            _ => Err(ClientError::UnexpectedMessage),
        }
    }

    /// Verify that the daemon is responsive.
    pub fn ping(&mut self) -> Result<(), ClientError> {
        match self.request(Command::Ping)? {
            ResponseResult::Pong => Ok(()),
            _ => Err(ClientError::UnexpectedMessage),
        }
    }

    /// Subscribe to daemon notifications.
    pub fn subscribe(&mut self) -> Result<(), ClientError> {
        match self.request(Command::Subscribe {
            subscriptions: vec![Subscription::All],
        })? {
            ResponseResult::Subscribed => Ok(()),
            _ => Err(ClientError::UnexpectedMessage),
        }
    }

    /// Stop receiving daemon notifications.
    pub fn unsubscribe(&mut self) -> Result<(), ClientError> {
        match self.request(Command::Unsubscribe {
            subscriptions: vec![Subscription::All],
        })? {
            ResponseResult::Unsubscribed => Ok(()),
            _ => Err(ClientError::UnexpectedMessage),
        }
    }

    /// Request reactor timing and diagnostic loss counters (protocol 1.1).
    pub fn diagnostics(&mut self) -> Result<crate::Diagnostics, ClientError> {
        self.require_minor(1)?;
        match self.request(Command::Diagnostics)? {
            ResponseResult::Diagnostics(value) => Ok(value),
            _ => Err(ClientError::UnexpectedMessage),
        }
    }
    /// Return the running daemon's validated configuration, in canonical text form.
    pub fn config(&mut self) -> Result<String, ClientError> {
        self.require_minor(1)?;
        match self.request(Command::Config)? {
            ResponseResult::Config(value) => Ok(value),
            _ => Err(ClientError::UnexpectedMessage),
        }
    }
    /// Open additional sensors for this connection. Subscribe before capturing.
    /// Up to 32 devices may be leased; stop_capture releases every lease.
    pub fn start_capture(&mut self, syspath: &str) -> Result<DeviceInfo, ClientError> {
        self.require_minor(1)?;
        match self.request(Command::StartCapture {
            syspath: syspath.to_owned(),
        })? {
            ResponseResult::CaptureStarted(value) => Ok(value),
            _ => Err(ClientError::UnexpectedMessage),
        }
    }
    pub fn stop_capture(&mut self) -> Result<(), ClientError> {
        self.require_minor(1)?;
        match self.request(Command::StopCapture)? {
            ResponseResult::CaptureStopped => Ok(()),
            _ => Err(ClientError::UnexpectedMessage),
        }
    }

    /// Return the next queued notification, blocking until one is available.
    pub fn next_event(&mut self) -> Result<Notification, ClientError> {
        loop {
            if let Some(notification) = self.poll_event()? {
                return Ok(notification);
            }
        }
    }

    /// Return a queued notification or read at most one socket chunk.
    ///
    /// `None` means a partial frame was retained for the next call. Each socket
    /// read observes the configured timeout; callers can check cancellation and
    /// absolute deadlines between calls even when a peer trickles frame bytes.
    pub fn poll_event(&mut self) -> Result<Option<Notification>, ClientError> {
        self.ensure_active()?;
        if let Some((notification, frame_bytes)) = self.notifications.pop_front() {
            self.notification_bytes -= frame_bytes;
            return Ok(Some(notification));
        }
        let Some((message, _)) = self.poll_message()? else {
            return Ok(None);
        };
        match message {
            ServerMessage::Notification(notification) => Ok(Some(notification)),
            ServerMessage::Response { .. } => Err(ClientError::UnexpectedMessage),
            ServerMessage::Error { error, .. } => Err(ClientError::Server { error }),
        }
    }

    pub fn protocol_version(&self) -> (u16, u16) {
        (PROTOCOL_MAJOR, self.protocol_minor)
    }
    fn require_minor(&self, required_minor: u16) -> Result<(), ClientError> {
        if self.protocol_minor < required_minor {
            Err(ClientError::UnsupportedFeature {
                peer_minor: self.protocol_minor,
                required_minor,
            })
        } else {
            Ok(())
        }
    }

    fn hello(&mut self) -> Result<(), ClientError> {
        match self.request(Command::Hello {
            min_major: PROTOCOL_MAJOR,
            max_major: PROTOCOL_MAJOR,
        })? {
            ResponseResult::Hello {
                major,
                minor,
                daemon_version: _,
            } if major == PROTOCOL_MAJOR => {
                self.protocol_minor = minor;
                Ok(())
            }
            ResponseResult::Hello { major, minor, .. } => {
                Err(ClientError::UnsupportedVersion { major, minor })
            }
            _ => Err(ClientError::UnexpectedMessage),
        }
    }

    fn request(&mut self, command: Command) -> Result<ResponseResult, ClientError> {
        self.ensure_active()?;
        let id = self.allocate_id();
        let request = Request { id, command };
        let frame = encode_frame(&request)?;
        self.stream.write_all(&frame).map_err(map_stream_io_error)?;
        let timeout = self.stream.read_timeout()?;
        let deadline = timeout.and_then(|timeout| Instant::now().checked_add(timeout));
        let result = self.read_response(id, deadline);
        // A deadline only narrows this request's reads. Restore the caller's
        // timeout on success and failure before returning control.
        let restored = self.stream.set_read_timeout(timeout);
        match result {
            Ok(response) => {
                restored?;
                Ok(response)
            }
            Err(error) => Err(error),
        }
    }

    fn read_response(
        &mut self,
        id: u64,
        deadline: Option<Instant>,
    ) -> Result<ResponseResult, ClientError> {
        loop {
            if let Some(deadline) = deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "IPC response deadline exceeded",
                    )
                    .into());
                }
                self.stream.set_read_timeout(Some(remaining))?;
            }
            let Some((message, frame_bytes)) = self.poll_message()? else {
                continue;
            };
            match message {
                ServerMessage::Notification(notification) => {
                    self.queue_notification(notification, frame_bytes)?;
                }
                ServerMessage::Response {
                    id: response_id,
                    result,
                } => {
                    if response_id != id {
                        return Err(ClientError::UnexpectedResponseId {
                            expected: id,
                            actual: response_id,
                        });
                    }
                    return Ok(result);
                }
                ServerMessage::Error {
                    id: error_id,
                    error,
                } => {
                    if let Some(actual) = error_id
                        && actual != id
                    {
                        return Err(ClientError::UnexpectedResponseId {
                            expected: id,
                            actual,
                        });
                    }
                    return Err(ClientError::Server { error });
                }
            }
        }
    }

    fn ensure_active(&self) -> Result<(), ClientError> {
        if self.terminated {
            Err(ClientError::NotificationBacklogExceeded {
                limit: MAX_NOTIFICATION_BACKLOG_BYTES,
            })
        } else {
            Ok(())
        }
    }

    fn queue_notification(
        &mut self,
        notification: Notification,
        frame_bytes: usize,
    ) -> Result<(), ClientError> {
        let Some(notification_bytes) = self
            .notification_bytes
            .checked_add(frame_bytes)
            .filter(|&bytes| bytes <= MAX_NOTIFICATION_BACKLOG_BYTES)
        else {
            self.terminated = true;
            return Err(ClientError::NotificationBacklogExceeded {
                limit: MAX_NOTIFICATION_BACKLOG_BYTES,
            });
        };
        self.notification_bytes = notification_bytes;
        self.notifications.push_back((notification, frame_bytes));
        Ok(())
    }

    fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        id
    }

    fn poll_message(&mut self) -> Result<Option<(ServerMessage, usize)>, ClientError> {
        if let Some(message) = self.messages.pop_front() {
            return Ok(Some(message));
        }
        let mut bytes = [0u8; 8192];
        let count = self.stream.read(&mut bytes).map_err(map_stream_io_error)?;
        if count == 0 {
            return Err(ClientError::PrematureEof);
        }
        let buffered_bytes = self.frames.len();
        let frames = self
            .frames
            .push(&bytes[..count])
            .map_err(ClientError::Frame)?;
        let mut received_chunks = bytes[..count].split_inclusive(|&byte| byte == b'\n');
        let mut prefix_bytes = buffered_bytes;
        for frame in frames {
            let received_chunk = received_chunks
                .next()
                .expect("each completed frame has a received delimiter");
            let frame_bytes = prefix_bytes + received_chunk.len();
            prefix_bytes = 0;
            self.messages.push_back((
                decode_frame(&frame).map_err(ClientError::Frame)?,
                frame_bytes,
            ));
        }
        Ok(self.messages.pop_front())
    }
}

fn map_stream_io_error(error: io::Error) -> ClientError {
    match error.kind() {
        io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::UnexpectedEof => ClientError::PrematureEof,
        _ => ClientError::Io(error),
    }
}

/// Return the default daemon socket path.
pub fn default_socket_path() -> Result<PathBuf, ClientError> {
    let runtime = env::var_os("XDG_RUNTIME_DIR").ok_or(ClientError::RuntimeDirectoryMissing)?;
    let runtime = PathBuf::from(runtime);
    if !runtime.is_absolute() {
        return Err(ClientError::RuntimeDirectoryRelative(runtime));
    }
    Ok(runtime.join("wiiland").join("wiilandd.sock"))
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    #[test]
    fn request_restores_the_configured_read_timeout_on_success_and_failure() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let timeout = Some(Duration::from_millis(100));
        stream.set_read_timeout(timeout).unwrap();
        let mut client = Client {
            stream,
            socket_path: PathBuf::new(),
            frames: FrameBuffer::new(),
            notifications: VecDeque::new(),
            notification_bytes: 0,
            messages: VecDeque::new(),
            terminated: false,
            next_id: 1,
            protocol_minor: 1,
        };
        let server = std::thread::spawn(move || {
            let mut reader = BufReader::new(peer.try_clone().unwrap());
            let mut frame = Vec::new();
            reader.read_until(b'\n', &mut frame).unwrap();
            let request: Request = decode_frame(&frame).unwrap();
            let response = encode_frame(&ServerMessage::Response {
                id: request.id,
                result: ResponseResult::Pong,
            })
            .unwrap();
            std::thread::sleep(Duration::from_millis(20));
            peer.write_all(&response[..5]).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            peer.write_all(&response[5..]).unwrap();
            frame.clear();
            reader.read_until(b'\n', &mut frame).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            peer.write_all(b"{").unwrap();
            std::thread::sleep(Duration::from_millis(150));
        });
        client.ping().unwrap();
        assert_eq!(client.stream.read_timeout().unwrap(), timeout);
        assert!(
            matches!(client.ping(), Err(ClientError::Io(error)) if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock))
        );
        assert_eq!(client.stream.read_timeout().unwrap(), timeout);
        drop(client);
        server.join().unwrap();
    }
}
