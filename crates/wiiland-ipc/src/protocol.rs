//! Wire-level types and bounded newline-delimited JSON framing.

use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use std::{error::Error, fmt};

/// Protocol major version.
pub const PROTOCOL_MAJOR: u16 = 1;
/// Protocol minor version.
pub const PROTOCOL_MINOR: u16 = 1;
/// Maximum sensor capture leases per connection.
pub const MAX_CAPTURE_DEVICES: usize = 32;
/// Maximum JSON frame size, excluding its delimiter.
pub const MAX_FRAME_BYTES: usize = 65_536;

/// A request sent by a client.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub command: Command,
}

/// Commands understood by the daemon.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Hello {
        min_major: u16,
        max_major: u16,
    },
    Ping,
    Diagnostics,
    Config,
    /// Lease all readable sensor interfaces until StopCapture or disconnect.
    StartCapture {
        syspath: String,
    },
    StopCapture,
    Status,
    Devices,
    Subscribe {
        subscriptions: Vec<Subscription>,
    },
    Unsubscribe {
        subscriptions: Vec<Subscription>,
    },
    #[serde(other)]
    Unknown,
}

/// A message sent by the daemon.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ServerMessage {
    Response {
        id: u64,
        result: ResponseResult,
    },
    Notification(Notification),
    Error {
        id: Option<u64>,
        error: ProtocolError,
    },
}

/// Successful command results.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ResponseResult {
    Hello {
        major: u16,
        minor: u16,
        daemon_version: String,
    },
    Pong,
    Diagnostics(Diagnostics),
    Config(String),
    CaptureStarted(DeviceInfo),
    CaptureStopped,
    Status(Status),
    Devices(Vec<DeviceInfo>),
    Subscribed,
    Unsubscribed,
}

/// A protocol error returned by the daemon, also used for bounded framing
/// failures so callers have one error type for the wire boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: ProtocolErrorCode,
    pub message: String,
}

impl ProtocolError {
    #[allow(non_upper_case_globals)]
    pub const FrameTooLarge: Self = Self {
        code: ProtocolErrorCode::FrameTooLarge,
        message: String::new(),
    };

    fn frame_too_large(size: usize) -> Self {
        Self {
            code: ProtocolErrorCode::FrameTooLarge,
            message: format!("JSON frame is {size} bytes, maximum is {MAX_FRAME_BYTES}"),
        }
    }

    fn json(error: serde_json::Error) -> Self {
        Self {
            code: ProtocolErrorCode::InvalidJson,
            message: error.to_string(),
        }
    }
}

/// Stable machine-readable protocol error codes.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    UnsupportedVersion,
    InvalidRequest,
    UnknownCommand,
    NotSubscribed,
    FrameTooLarge,
    InvalidJson,
    Internal,
    #[serde(other)]
    Unknown,
}

/// Authoritative daemon status at the time of a request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub daemon_version: String,
    pub pid: u32,
    pub device_count: u32,
    pub dry_run: bool,
    pub socket_path: String,
}

/// Reactor health counters, sampled without blocking device ingestion.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Diagnostics {
    pub trace_records_dropped: u64,
    pub lifecycle_records_dropped: u64,
    pub max_pointer_lateness_us: u64,
    pub max_dispatch_duration_us: u64,
}

/// Information about an opened or known device.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub syspath: String,
    pub profile: Profile,
    pub opened_interfaces: u32,
    pub pending_interfaces: u32,
    pub gamepad_output: bool,
    pub desktop_output: bool,
}

/// Device profile names used by the daemon.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    None,
    Gamepad,
    Desktop,
    Both,
    #[serde(other)]
    Unknown,
}

/// A subscription stream supported by the protocol.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subscription {
    All,
    Input,
    Devices,
}

/// Subscription flags, useful to language-neutral implementations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Subscriptions {
    pub input: bool,
    pub devices: bool,
}

/// An unsolicited daemon event. `sequence` is monotonic per connection.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Notification {
    DeviceAdded {
        sequence: u64,
        device: DeviceInfo,
    },
    DeviceRemoved {
        sequence: u64,
        syspath: String,
        reason: RemovalReason,
    },
    Input {
        sequence: u64,
        syspath: String,
        timestamp: Timestamp,
        payload: InputPayload,
    },
    #[serde(other)]
    Unsupported,
}

/// Why a device was removed from the daemon.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalReason {
    Removed,
    Gone,
    DrainError,
    PointerError,
    #[serde(other)]
    Unknown,
}

/// Kernel-provided input event timestamp, represented without libc types.
/// The clock source is not part of the wire contract.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Timestamp {
    #[serde(alias = "sec")]
    pub seconds: i64,
    #[serde(alias = "usec")]
    pub micros: u32,
}

/// Three signed, fixed-width axes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Axis3 {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

/// A button transition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ButtonEvent {
    pub code: u32,
    pub state: u32,
}

/// Semantic input reports emitted by the daemon.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum InputPayload {
    Key(ButtonEvent),
    Accel(Axis3),
    Ir([Axis3; 4]),
    BalanceBoard([Axis3; 4]),
    MotionPlus(Axis3),
    ProControllerKey(ButtonEvent),
    ProControllerMove([Axis3; 2]),
    Watch,
    ClassicControllerKey(ButtonEvent),
    ClassicControllerMove([Axis3; 3]),
    NunchukKey(ButtonEvent),
    NunchukMove([Axis3; 2]),
    DrumsKey(ButtonEvent),
    DrumsMove([Axis3; 8]),
    GuitarKey(ButtonEvent),
    GuitarMove([Axis3; 3]),
    Gone,
    Unknown(u32),
    #[serde(other)]
    Unsupported,
}

impl InputPayload {
    /// Stable event number used by the trace contract.
    pub fn event_code(&self) -> u32 {
        match self {
            Self::Key(_) => 0,
            Self::Accel(_) => 1,
            Self::Ir(_) => 2,
            Self::BalanceBoard(_) => 3,
            Self::MotionPlus(_) => 4,
            Self::ProControllerKey(_) => 5,
            Self::ProControllerMove(_) => 6,
            Self::Watch => 7,
            Self::ClassicControllerKey(_) => 8,
            Self::ClassicControllerMove(_) => 9,
            Self::NunchukKey(_) => 10,
            Self::NunchukMove(_) => 11,
            Self::DrumsKey(_) => 12,
            Self::DrumsMove(_) => 13,
            Self::GuitarKey(_) => 14,
            Self::GuitarMove(_) => 15,
            Self::Gone => 16,
            Self::Unknown(value) => *value,
            Self::Unsupported => u32::MAX,
        }
    }
}

impl<'de> Deserialize<'de> for InputPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct TaggedPayload {
            #[serde(rename = "type")]
            kind: String,
            value: Option<serde_json::Value>,
        }

        let TaggedPayload { kind, value } = TaggedPayload::deserialize(deserializer)?;
        match kind.as_str() {
            "key" => decode_input_value(value).map(Self::Key),
            "accel" => decode_input_value(value).map(Self::Accel),
            "ir" => decode_input_value(value).map(Self::Ir),
            "balance_board" => decode_input_value(value).map(Self::BalanceBoard),
            "motion_plus" => decode_input_value(value).map(Self::MotionPlus),
            "pro_controller_key" => decode_input_value(value).map(Self::ProControllerKey),
            "pro_controller_move" => decode_input_value(value).map(Self::ProControllerMove),
            "watch" => decode_unit_input(value, Self::Watch),
            "classic_controller_key" => decode_input_value(value).map(Self::ClassicControllerKey),
            "classic_controller_move" => decode_input_value(value).map(Self::ClassicControllerMove),
            "nunchuk_key" => decode_input_value(value).map(Self::NunchukKey),
            "nunchuk_move" => decode_input_value(value).map(Self::NunchukMove),
            "drums_key" => decode_input_value(value).map(Self::DrumsKey),
            "drums_move" => decode_input_value(value).map(Self::DrumsMove),
            "guitar_key" => decode_input_value(value).map(Self::GuitarKey),
            "guitar_move" => decode_input_value(value).map(Self::GuitarMove),
            "gone" => decode_unit_input(value, Self::Gone),
            "unknown" => decode_input_value(value).map(Self::Unknown),
            _ => Ok(Self::Unsupported),
        }
    }
}

fn decode_input_value<T, E>(value: Option<serde_json::Value>) -> Result<T, E>
where
    T: DeserializeOwned,
    E: serde::de::Error,
{
    let value = value.ok_or_else(|| E::missing_field("value"))?;
    serde_json::from_value(value).map_err(E::custom)
}

fn decode_unit_input<E>(
    value: Option<serde_json::Value>,
    payload: InputPayload,
) -> Result<InputPayload, E>
where
    E: serde::de::Error,
{
    if value.is_some() {
        return Err(E::custom("unexpected value for unit input payload"));
    }
    Ok(payload)
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            write!(f, "{:?}", self.code)
        } else {
            write!(f, "{:?}: {}", self.code, self.message)
        }
    }
}

impl Error for ProtocolError {}

/// Name retained for callers that refer specifically to frame errors.
pub type FrameError = ProtocolError;

/// Serialize one JSON object and append its newline delimiter.
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    let json = serde_json::to_vec(value).map_err(ProtocolError::json)?;
    if json.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::frame_too_large(json.len()));
    }
    let mut frame = json;
    frame.push(b'\n');
    Ok(frame)
}

/// Decode one JSON frame. A trailing LF or CRLF is accepted for convenience.
pub fn decode_frame<T: DeserializeOwned>(frame: &[u8]) -> Result<T, ProtocolError> {
    let mut payload = frame;
    if payload.last() == Some(&b'\n') {
        payload = &payload[..payload.len() - 1];
    }
    if payload.last() == Some(&b'\r') {
        payload = &payload[..payload.len() - 1];
    }
    if payload.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::frame_too_large(payload.len()));
    }
    serde_json::from_slice(payload).map_err(ProtocolError::json)
}

/// Incrementally collect newline-delimited frames without growing past the limit.
#[derive(Clone, Debug, Default)]
pub struct FrameBuffer {
    bytes: Vec<u8>,
}

impl FrameBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append bytes and return every complete frame payload (without LF/CRLF).
    pub fn push(&mut self, mut input: &[u8]) -> Result<Vec<Vec<u8>>, ProtocolError> {
        let mut frames = Vec::new();
        while !input.is_empty() {
            if let Some(newline) = input.iter().position(|&byte| byte == b'\n') {
                let part = &input[..newline];
                let frame_len = self.bytes.len() + part.len();
                let payload_len = frame_len
                    - usize::from(
                        part.last() == Some(&b'\r')
                            || (part.is_empty() && self.bytes.last() == Some(&b'\r')),
                    );
                if payload_len > MAX_FRAME_BYTES {
                    return Err(ProtocolError::frame_too_large(payload_len));
                }
                self.bytes.extend_from_slice(part);
                if self.bytes.last() == Some(&b'\r') {
                    self.bytes.pop();
                }
                frames.push(std::mem::take(&mut self.bytes));
                input = &input[newline + 1..];
            } else {
                let frame_len = self.bytes.len() + input.len();
                let payload_len = frame_len - usize::from(input.last() == Some(&b'\r'));
                if payload_len > MAX_FRAME_BYTES {
                    return Err(ProtocolError::frame_too_large(payload_len));
                }
                self.bytes.extend_from_slice(input);
                break;
            }
        }
        Ok(frames)
    }

    /// Alias for callers that use stream terminology.
    pub fn feed(&mut self, input: &[u8]) -> Result<Vec<Vec<u8>>, ProtocolError> {
        self.push(input)
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_hello_json() {
        let request = Request {
            id: 7,
            command: Command::Hello {
                min_major: PROTOCOL_MAJOR,
                max_major: PROTOCOL_MAJOR,
            },
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"id":7,"command":{"type":"hello","min_major":1,"max_major":1}}"#
        );
    }

    #[test]
    fn partial_framing() {
        let mut buffer = FrameBuffer::new();
        assert!(
            buffer
                .push(br#"{"id":1,"command":{"type":"ping"}}"#)
                .unwrap()
                .is_empty()
        );
        let frames = buffer.push(b"\n").unwrap();
        assert_eq!(
            frames,
            vec![br#"{"id":1,"command":{"type":"ping"}}"#.to_vec()]
        );
    }

    #[test]
    fn crlf_is_tolerated() {
        let mut buffer = FrameBuffer::new();
        let frames = buffer.push(b"{}\r\n").unwrap();
        assert_eq!(frames, vec![b"{}".to_vec()]);
        let _: serde_json::Value = decode_frame(b"{}\r\n").unwrap();
    }

    #[test]
    fn exact_max_payload_with_lf_is_accepted() {
        let payload = vec![b'a'; MAX_FRAME_BYTES];
        let mut frame = payload.clone();
        frame.push(b'\n');

        let mut buffer = FrameBuffer::new();
        assert_eq!(buffer.push(&frame).unwrap(), vec![payload]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn exact_max_payload_with_crlf_is_accepted() {
        let payload = vec![b'a'; MAX_FRAME_BYTES];
        let mut frame = payload.clone();
        frame.extend_from_slice(b"\r\n");

        let mut buffer = FrameBuffer::new();
        assert_eq!(buffer.push(&frame).unwrap(), vec![payload.clone()]);
        assert!(buffer.push(&payload).unwrap().is_empty());
        assert!(buffer.push(b"\r").unwrap().is_empty());
        assert_eq!(buffer.len(), MAX_FRAME_BYTES + 1);
        assert_eq!(buffer.push(b"\n").unwrap(), vec![payload]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn max_plus_one_payload_with_lf_or_crlf_is_rejected() {
        for delimiter in [b"\n".as_slice(), b"\r\n".as_slice()] {
            let mut frame = vec![b'a'; MAX_FRAME_BYTES + 1];
            frame.extend_from_slice(delimiter);

            let error = FrameBuffer::new().push(&frame).unwrap_err();
            assert_eq!(error.code, ProtocolErrorCode::FrameTooLarge);
            assert!(error.message.contains("65537 bytes"));
        }
    }

    #[test]
    fn chunked_stream_rejects_payload_overflow_immediately() {
        let payload = vec![b'a'; MAX_FRAME_BYTES];
        let mut buffer = FrameBuffer::new();
        assert!(buffer.push(&payload).unwrap().is_empty());

        let error = buffer.push(b"x").unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::FrameTooLarge);
        assert!(error.message.contains("65537 bytes"));
        assert_eq!(buffer.len(), MAX_FRAME_BYTES);

        let mut buffer = FrameBuffer::new();
        assert!(buffer.push(&payload).unwrap().is_empty());
        assert!(buffer.push(b"\r").unwrap().is_empty());
        assert_eq!(buffer.len(), MAX_FRAME_BYTES + 1);

        let error = buffer.push(b"x").unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::FrameTooLarge);
        assert_eq!(buffer.len(), MAX_FRAME_BYTES + 1);
    }

    #[test]
    fn oversized_encoded_value_is_rejected() {
        let value = vec![b'a'; MAX_FRAME_BYTES];
        assert!(matches!(
            encode_frame(&value),
            Err(ProtocolError {
                code: ProtocolErrorCode::FrameTooLarge,
                ..
            })
        ));
    }

    #[test]
    fn unknown_fields_are_accepted() {
        let request: Request =
            serde_json::from_str(r#"{"id":3,"command":{"type":"ping","future":true},"future":42}"#)
                .unwrap();
        assert_eq!(request.command, Command::Ping);
    }

    #[test]
    fn golden_correlated_error_json() {
        let message = ServerMessage::Error {
            id: Some(11),
            error: ProtocolError {
                code: ProtocolErrorCode::UnknownCommand,
                message: "unsupported command".into(),
            },
        };
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            r#"{"type":"error","value":{"id":11,"error":{"code":"unknown_command","message":"unsupported command"}}}"#
        );
    }

    #[test]
    fn scalar_wire_enums_use_snake_case() {
        assert_eq!(
            serde_json::to_string(&ProtocolErrorCode::UnsupportedVersion).unwrap(),
            r#""unsupported_version""#
        );
        assert_eq!(
            serde_json::to_string(&Subscription::Input).unwrap(),
            r#""input""#
        );
    }

    #[test]
    fn unknown_command_tags_are_representable() {
        let request: Request = serde_json::from_str(
            r#"{"id":12,"command":{"type":"future_command","argument":true}}"#,
        )
        .unwrap();
        assert_eq!(request.command, Command::Unknown);
    }
}
