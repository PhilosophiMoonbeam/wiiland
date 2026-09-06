//! Per-connection framing, subscriptions, and bounded output.
use super::{FRAME_BUDGET, MAX_QUEUED_BYTES, READ_BUDGET, READ_CHUNK, WRITE_BUDGET};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
};
use wiiland_ipc::{
    Command, DeviceInfo, FrameBuffer, Notification, PROTOCOL_MAJOR, PROTOCOL_MINOR, ProtocolError,
    ProtocolErrorCode, Request, ResponseResult, ServerMessage, Status, Subscription, encode_frame,
};
#[derive(Debug)]
pub(super) struct Pending {
    pub(super) bytes: Vec<u8>,
    pub(super) offset: usize,
}

#[derive(Debug)]
pub(super) struct Client {
    pub(super) stream: UnixStream,
    pub(super) frames: FrameBuffer,
    pub(super) pending_frames: VecDeque<Vec<u8>>,
    pub(super) output: VecDeque<Pending>,
    pub(super) queued_bytes: usize,
    pub(super) negotiated: bool,
    pub(super) immediate_close: bool,
    pub(super) closing: bool,
    pub(super) input: bool,
    pub(super) devices: bool,
    pub(super) commands: VecDeque<(u64, Command)>,
    pub(super) capture: Vec<String>,
}

impl Client {
    pub(super) fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            frames: FrameBuffer::new(),
            pending_frames: VecDeque::new(),
            output: VecDeque::new(),
            queued_bytes: 0,
            negotiated: false,
            immediate_close: false,
            closing: false,
            input: false,
            devices: false,
            commands: VecDeque::new(),
            capture: Vec::new(),
        }
    }

    pub(super) fn wants(&self, notification: &Notification) -> bool {
        match notification {
            Notification::Input { .. } => self.input,
            Notification::DeviceAdded { .. } | Notification::DeviceRemoved { .. } => self.devices,
            _ => false,
        }
    }

    pub(super) fn queue(&mut self, message: &ServerMessage) -> bool {
        let Ok(bytes) = encode_frame(message) else {
            self.immediate_close = true;
            return false;
        };
        self.queue_encoded(bytes)
    }

    pub(super) fn queue_response(&mut self, id: u64, result: ResponseResult) -> bool {
        let message = ServerMessage::Response { id, result };
        match encode_frame(&message) {
            Ok(bytes) => self.queue_encoded(bytes),
            Err(_) => self.queue(&ServerMessage::Error {
                id: Some(id),
                error: protocol_error(
                    ProtocolErrorCode::Internal,
                    "response exceeds the maximum IPC frame size",
                ),
            }),
        }
    }

    pub(super) fn queue_encoded(&mut self, bytes: Vec<u8>) -> bool {
        if self.queued_bytes.saturating_add(bytes.len()) > MAX_QUEUED_BYTES {
            self.immediate_close = true;
            return false;
        }
        self.queued_bytes += bytes.len();
        self.output.push_back(Pending { bytes, offset: 0 });
        true
    }

    pub(super) fn write_ready(&mut self) -> bool {
        let mut budget = WRITE_BUDGET;
        while budget != 0 {
            let Some(front) = self.output.front_mut() else {
                break;
            };
            let remaining = &front.bytes[front.offset..];
            match self.stream.write(&remaining[..remaining.len().min(budget)]) {
                Ok(0) => {
                    self.closing = true;
                    return false;
                }
                Ok(written) => {
                    front.offset += written;
                    self.queued_bytes = self.queued_bytes.saturating_sub(written);
                    budget -= written;
                    if front.offset == front.bytes.len() {
                        self.output.pop_front();
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.closing = true;
                    return false;
                }
            }
        }
        true
    }
}

/// Single-threaded, nonblocking Unix-socket IPC server.
///
pub(super) fn read_client(
    client: &mut Client,
    socket_path: &Path,
    status: &mut dyn FnMut(&Path) -> Status,
    devices: &mut dyn FnMut() -> Vec<DeviceInfo>,
) -> bool {
    let mut frame_budget = FRAME_BUDGET;
    if !process_pending(client, socket_path, status, devices, &mut frame_budget) {
        return false;
    }
    if frame_budget == 0
        || !client.pending_frames.is_empty()
        || client.closing
        || client.immediate_close
    {
        return true;
    }
    let mut scratch = [0u8; READ_CHUNK];
    for _ in 0..READ_BUDGET {
        match client.stream.read(&mut scratch) {
            Ok(0) => return false,
            Ok(size) => {
                let frames = match client.frames.push(&scratch[..size]) {
                    Ok(frames) => frames,
                    Err(error) => {
                        client.closing = true;
                        let _ = client.queue(&ServerMessage::Error { id: None, error });
                        return true;
                    }
                };
                client.pending_frames.extend(frames);
                if !process_pending(client, socket_path, status, devices, &mut frame_budget) {
                    return false;
                }
                if client.closing
                    || client.immediate_close
                    || frame_budget == 0
                    || !client.pending_frames.is_empty()
                    || size < scratch.len()
                {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => return false,
        }
    }
    true
}

pub(super) fn process_pending(
    client: &mut Client,
    socket_path: &Path,
    status: &mut dyn FnMut(&Path) -> Status,
    devices: &mut dyn FnMut() -> Vec<DeviceInfo>,
    budget: &mut usize,
) -> bool {
    while *budget != 0 {
        let Some(frame) = client.pending_frames.pop_front() else {
            break;
        };
        *budget -= 1;
        if !handle_frame(client, &frame, socket_path, status, devices) {
            return false;
        }
        if client.closing || client.immediate_close {
            break;
        }
    }
    true
}
pub(super) fn handle_frame(
    client: &mut Client,
    frame: &[u8],
    socket_path: &Path,
    status: &mut dyn FnMut(&Path) -> Status,
    devices: &mut dyn FnMut() -> Vec<DeviceInfo>,
) -> bool {
    let request: Request = match wiiland_ipc::decode_frame(frame) {
        Ok(request) => request,
        Err(error) => {
            client.queue(&ServerMessage::Error { id: None, error });
            return true;
        }
    };
    let id = request.id;
    if !client.negotiated {
        match request.command {
            Command::Hello {
                min_major,
                max_major,
            } if min_major <= PROTOCOL_MAJOR && max_major >= PROTOCOL_MAJOR => {
                client.negotiated = true;
                client.queue_response(
                    id,
                    ResponseResult::Hello {
                        major: PROTOCOL_MAJOR,
                        minor: PROTOCOL_MINOR,
                        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                );
            }
            Command::Hello { .. } => {
                client.queue(&ServerMessage::Error {
                    id: Some(id),
                    error: protocol_error(
                        ProtocolErrorCode::UnsupportedVersion,
                        "unsupported protocol version",
                    ),
                });
                client.closing = true;
            }
            _ => {
                client.queue(&ServerMessage::Error {
                    id: Some(id),
                    error: protocol_error(
                        ProtocolErrorCode::InvalidRequest,
                        "first request must be hello",
                    ),
                });
                client.closing = true;
            }
        }
        return true;
    }

    match request.command {
        Command::Hello { .. } => {
            client.queue(&ServerMessage::Error {
                id: Some(id),
                error: protocol_error(
                    ProtocolErrorCode::InvalidRequest,
                    "hello was already completed",
                ),
            });
        }
        Command::Ping => {
            client.queue_response(id, ResponseResult::Pong);
        }
        Command::Status => {
            client.queue_response(id, ResponseResult::Status(status(socket_path)));
        }
        Command::Devices => {
            client.queue_response(id, ResponseResult::Devices(devices()));
        }
        Command::Subscribe { subscriptions } => {
            apply_subscriptions(client, &subscriptions, true);
            client.queue_response(id, ResponseResult::Subscribed);
        }
        Command::Unsubscribe { subscriptions } => {
            apply_subscriptions(client, &subscriptions, false);
            client.queue_response(id, ResponseResult::Unsubscribed);
        }
        command @ (Command::Diagnostics
        | Command::Config
        | Command::StartCapture { .. }
        | Command::StopCapture) => {
            if matches!(&command, Command::StartCapture { syspath } if client.capture.len() >= wiiland_ipc::MAX_CAPTURE_DEVICES && !client.capture.contains(syspath))
            {
                client.queue(&ServerMessage::Error {
                    id: Some(id),
                    error: protocol_error(
                        ProtocolErrorCode::InvalidRequest,
                        "capture device limit reached; stop capture before selecting new devices",
                    ),
                });
            } else if client.commands.len() >= FRAME_BUDGET {
                client.immediate_close = true;
            } else {
                client.commands.push_back((id, command));
            }
        }
        Command::Unknown => {
            client.queue(&ServerMessage::Error {
                id: Some(id),
                error: protocol_error(ProtocolErrorCode::UnknownCommand, "unknown command"),
            });
        }
        _ => {
            client.queue(&ServerMessage::Error {
                id: Some(id),
                error: protocol_error(ProtocolErrorCode::UnknownCommand, "unknown command"),
            });
        }
    }
    true
}

pub(super) fn protocol_error(code: ProtocolErrorCode, message: &str) -> ProtocolError {
    ProtocolError {
        code,
        message: message.to_owned(),
    }
}

pub(super) fn apply_subscriptions(
    client: &mut Client,
    subscriptions: &[Subscription],
    enabled: bool,
) {
    for subscription in subscriptions {
        match subscription {
            Subscription::All => {
                client.input = enabled;
                client.devices = enabled;
            }
            Subscription::Input => client.input = enabled,
            Subscription::Devices => client.devices = enabled,
            _ => {}
        }
    }
}
