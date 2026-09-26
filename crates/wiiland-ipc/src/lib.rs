#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

//! Rust facade for the wiiland daemon's platform IPC.
//!
//! This crate provides owned Rust DTOs and a bounded newline-delimited JSON
//! codec, plus a blocking client for communicating with the daemon. It does
//! not directly own or access hardware: direct hardware ownership remains with
//! `wiiland-hid`, while this crate communicates with the daemon over IPC.
//!
//! This Rust API describes the IPC contract; it does not promise publication
//! as a standalone package or a stable binary ABI. It intentionally exposes no
//! libc, C ABI, executable, or daemon implementation details. [`Client`] is
//! the blocking platform-IPC facade.

mod capture;
mod client;
mod protocol;
mod session;
pub mod windows_bootstrap;
#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_transport;

pub use capture::CaptureConnection;
pub use client::{Client, ClientError, default_socket_path};
pub use protocol::{
    Axis3, ButtonEvent, Command, DeviceInfo, Diagnostics, FrameBuffer, FrameError, InputPayload,
    MAX_CAPTURE_DEVICES, MAX_FRAME_BYTES, Notification, PROTOCOL_MAJOR, PROTOCOL_MINOR, Profile,
    ProtocolError, ProtocolErrorCode, RemovalReason, Request, ResponseResult, ServerMessage,
    Status, Subscription, Subscriptions, Timestamp, decode_frame, encode_frame,
};
pub use session::{Session, SessionEvent, select_devices};
