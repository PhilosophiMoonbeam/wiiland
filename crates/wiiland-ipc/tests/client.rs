use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Mutex;
use std::thread;

use wiiland_ipc::{
    Client, ClientError, Command, DeviceInfo, Notification, PROTOCOL_MAJOR, PROTOCOL_MINOR,
    Profile, ProtocolError, ProtocolErrorCode, RemovalReason, Request, ResponseResult,
    ServerMessage, Status, Subscription, decode_frame, encode_frame,
};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn socket_path(name: &str) -> PathBuf {
    let unique = format!(
        "wiiland-ipc-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    std::env::temp_dir().join(unique).join("wiilandd.sock")
}

fn start_server(name: &str) -> (PathBuf, UnixListener) {
    let path = socket_path(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&path).unwrap();
    (path, listener)
}

fn read_request(reader: &mut BufReader<UnixStream>) -> Request {
    let mut line = Vec::new();
    assert!(reader.read_until(b'\n', &mut line).unwrap() > 0);
    decode_frame(&line).unwrap()
}

fn send(stream: &mut UnixStream, message: ServerMessage) {
    stream.write_all(&encode_frame(&message).unwrap()).unwrap();
}

const NOTIFICATION_BACKLOG_BYTES: usize = 256 * 1024;
const LARGE_NOTIFICATION_FRAME_BYTES: usize = 64 * 1024;

fn notification_frame(sequence: u64, frame_bytes: usize) -> Vec<u8> {
    let message = |syspath| {
        ServerMessage::Notification(Notification::DeviceRemoved {
            sequence,
            syspath,
            reason: RemovalReason::Removed,
        })
    };
    let overhead = encode_frame(&message(String::new())).unwrap().len();
    assert!(frame_bytes >= overhead);
    let frame = encode_frame(&message("x".repeat(frame_bytes - overhead))).unwrap();
    assert_eq!(frame.len(), frame_bytes);
    frame
}

fn send_notification_frame(stream: &mut UnixStream, sequence: u64, frame_bytes: usize) {
    stream
        .write_all(&notification_frame(sequence, frame_bytes))
        .unwrap();
}

fn hello(id: u64, major: u16, minor: u16) -> ServerMessage {
    ServerMessage::Response {
        id,
        result: ResponseResult::Hello {
            major,
            minor,
            daemon_version: "test-daemon".into(),
        },
    }
}

fn status(id: u64) -> ServerMessage {
    ServerMessage::Response {
        id,
        result: ResponseResult::Status(Status {
            daemon_version: "test-daemon".into(),
            pid: 42,
            device_count: 1,
            dry_run: true,
            socket_path: "/tmp/test.sock".into(),
        }),
    }
}

fn protocol_error() -> ProtocolError {
    ProtocolError {
        code: ProtocolErrorCode::InvalidRequest,
        message: "request rejected".into(),
    }
}

fn device() -> DeviceInfo {
    DeviceInfo {
        syspath: "/sys/test".into(),
        profile: Profile::None,
        opened_interfaces: 1,
        pending_interfaces: 0,
        gamepad_output: true,
        desktop_output: false,
    }
}

#[test]
fn hello_status_devices_and_ping() {
    let (path, listener) = start_server("commands");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        assert!(matches!(request.command, Command::Hello { .. }));
        assert_ne!(request.id, 0);
        send(&mut stream, hello(request.id, PROTOCOL_MAJOR, 0));

        let request = read_request(&mut reader);
        assert!(matches!(request.command, Command::Status));
        send(&mut stream, status(request.id));

        let request = read_request(&mut reader);
        assert!(matches!(request.command, Command::Devices));
        send(
            &mut stream,
            ServerMessage::Response {
                id: request.id,
                result: ResponseResult::Devices(vec![device()]),
            },
        );

        let request = read_request(&mut reader);
        assert!(matches!(request.command, Command::Ping));
        send(
            &mut stream,
            ServerMessage::Response {
                id: request.id,
                result: ResponseResult::Pong,
            },
        );
    });

    let mut client = Client::connect(&path).unwrap();
    assert_eq!(client.protocol_version(), (1, 0));
    assert!(matches!(
        client.start_capture("/sys/test"),
        Err(ClientError::UnsupportedFeature {
            peer_minor: 0,
            required_minor: 1
        })
    ));
    assert_eq!(client.status().unwrap().pid, 42);
    assert_eq!(client.devices().unwrap().len(), 1);
    client.ping().unwrap();
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn notifications_are_queued_in_wire_order_while_waiting() {
    let (path, listener) = start_server("queue");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        send(&mut stream, hello(request.id, PROTOCOL_MAJOR, 0));
        let request = read_request(&mut reader);
        assert!(matches!(request.command, Command::Status));
        send(
            &mut stream,
            ServerMessage::Notification(Notification::DeviceRemoved {
                sequence: 1,
                syspath: "/sys/one".into(),
                reason: RemovalReason::Removed,
            }),
        );
        send(
            &mut stream,
            ServerMessage::Notification(Notification::DeviceRemoved {
                sequence: 2,
                syspath: "/sys/two".into(),
                reason: RemovalReason::Gone,
            }),
        );
        send(&mut stream, status(request.id));
    });

    let mut client = Client::connect(&path).unwrap();
    client.status().unwrap();
    match client.next_event().unwrap() {
        Notification::DeviceRemoved { syspath, .. } => assert_eq!(syspath, "/sys/one"),
        _ => panic!("unexpected first event"),
    }
    match client.next_event().unwrap() {
        Notification::DeviceRemoved { syspath, .. } => assert_eq!(syspath, "/sys/two"),
        _ => panic!("unexpected second event"),
    }
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn notification_backlog_accepts_exact_byte_limit() {
    let (path, listener) = start_server("backlog-exact");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        send(&mut stream, hello(request.id, PROTOCOL_MAJOR, 0));

        let request = read_request(&mut reader);
        assert!(matches!(request.command, Command::Status));
        for sequence in 1..=4 {
            send_notification_frame(&mut stream, sequence, LARGE_NOTIFICATION_FRAME_BYTES);
        }
        send(&mut stream, status(request.id));
    });

    let mut client = Client::connect(&path).unwrap();
    client.status().unwrap();
    for expected_sequence in 1..=4 {
        assert!(matches!(
            client.next_event().unwrap(),
            Notification::DeviceRemoved { sequence, .. } if sequence == expected_sequence
        ));
    }
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn notification_backlog_rejects_first_frame_over_limit() {
    let (path, listener) = start_server("backlog-crossing");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        send(&mut stream, hello(request.id, PROTOCOL_MAJOR, 0));

        let request = read_request(&mut reader);
        assert!(matches!(request.command, Command::Status));
        for sequence in 1..=4 {
            send_notification_frame(&mut stream, sequence, LARGE_NOTIFICATION_FRAME_BYTES);
        }
        send_notification_frame(&mut stream, 5, 1024);
        send(&mut stream, status(request.id));
    });

    let mut client = Client::connect(&path).unwrap();
    assert!(matches!(
        client.status(),
        Err(ClientError::NotificationBacklogExceeded {
            limit: NOTIFICATION_BACKLOG_BYTES
        })
    ));
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn notification_backlog_accounting_releases_popped_frame_bytes() {
    let (path, listener) = start_server("backlog-pop");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        send(&mut stream, hello(request.id, PROTOCOL_MAJOR, 0));

        let request = read_request(&mut reader);
        assert!(matches!(request.command, Command::Status));
        for sequence in 1..=4 {
            send_notification_frame(&mut stream, sequence, LARGE_NOTIFICATION_FRAME_BYTES);
        }
        send(&mut stream, status(request.id));

        let request = read_request(&mut reader);
        assert!(matches!(request.command, Command::Ping));
        send_notification_frame(&mut stream, 5, LARGE_NOTIFICATION_FRAME_BYTES);
        send(
            &mut stream,
            ServerMessage::Response {
                id: request.id,
                result: ResponseResult::Pong,
            },
        );
    });

    let mut client = Client::connect(&path).unwrap();
    client.status().unwrap();
    assert!(matches!(
        client.next_event().unwrap(),
        Notification::DeviceRemoved { sequence: 1, .. }
    ));
    client.ping().unwrap();
    for expected_sequence in 2..=5 {
        assert!(matches!(
            client.next_event().unwrap(),
            Notification::DeviceRemoved { sequence, .. } if sequence == expected_sequence
        ));
    }
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn notification_backlog_overflow_terminates_client() {
    let (path, listener) = start_server("backlog-terminal");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        send(&mut stream, hello(request.id, PROTOCOL_MAJOR, 0));

        let request = read_request(&mut reader);
        assert!(matches!(request.command, Command::Status));
        for sequence in 1..=4 {
            send_notification_frame(&mut stream, sequence, LARGE_NOTIFICATION_FRAME_BYTES);
        }
        send_notification_frame(&mut stream, 5, 1024);
        send(&mut stream, status(request.id));

        let mut unexpected_request = Vec::new();
        assert_eq!(
            reader.read_until(b'\n', &mut unexpected_request).unwrap(),
            0
        );
    });

    let mut client = Client::connect(&path).unwrap();
    assert!(matches!(
        client.status(),
        Err(ClientError::NotificationBacklogExceeded {
            limit: NOTIFICATION_BACKLOG_BYTES
        })
    ));
    assert!(matches!(
        client.next_event(),
        Err(ClientError::NotificationBacklogExceeded {
            limit: NOTIFICATION_BACKLOG_BYTES
        })
    ));
    assert!(matches!(
        client.ping(),
        Err(ClientError::NotificationBacklogExceeded {
            limit: NOTIFICATION_BACKLOG_BYTES
        })
    ));
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn subscribe_then_reads_event() {
    let (path, listener) = start_server("subscribe");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        send(&mut stream, hello(request.id, PROTOCOL_MAJOR, 0));
        let request = read_request(&mut reader);
        assert!(matches!(
            request.command,
            Command::Subscribe {
                subscriptions
            } if subscriptions == vec![Subscription::All]
        ));
        send(
            &mut stream,
            ServerMessage::Response {
                id: request.id,
                result: ResponseResult::Subscribed,
            },
        );
        send(
            &mut stream,
            ServerMessage::Notification(Notification::DeviceRemoved {
                sequence: 3,
                syspath: "/sys/event".into(),
                reason: RemovalReason::DrainError,
            }),
        );
    });

    let mut client = Client::connect(&path).unwrap();
    client.subscribe().unwrap();
    match client.next_event().unwrap() {
        Notification::DeviceRemoved { syspath, .. } => assert_eq!(syspath, "/sys/event"),
        _ => panic!("unexpected event"),
    }
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn supported_major_accepts_later_minor() {
    let (path, listener) = start_server("later-minor");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        let later_minor = PROTOCOL_MINOR
            .checked_add(1)
            .expect("protocol minor has room for a later version");
        send(&mut stream, hello(request.id, PROTOCOL_MAJOR, later_minor));
    });

    let client = Client::connect(&path).unwrap();
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn unsupported_hello_version_is_rejected() {
    let (path, listener) = start_server("version");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        send(&mut stream, hello(request.id, PROTOCOL_MAJOR + 1, 0));
    });

    let error = Client::connect(&path).err().unwrap();
    assert!(matches!(error, ClientError::UnsupportedVersion { .. }));
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn request_accepts_correlated_and_connection_level_errors() {
    let (path, listener) = start_server("request-errors");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        send(&mut stream, hello(request.id, PROTOCOL_MAJOR, 0));

        let request = read_request(&mut reader);
        assert!(matches!(request.command, Command::Status));
        send(
            &mut stream,
            ServerMessage::Error {
                id: Some(request.id),
                error: protocol_error(),
            },
        );

        let request = read_request(&mut reader);
        assert!(matches!(request.command, Command::Ping));
        send(
            &mut stream,
            ServerMessage::Error {
                id: None,
                error: protocol_error(),
            },
        );
    });

    let mut client = Client::connect(&path).unwrap();
    assert!(matches!(
        client.status(),
        Err(ClientError::Server {
            error: ProtocolError {
                code: ProtocolErrorCode::InvalidRequest,
                ..
            }
        })
    ));
    assert!(matches!(
        client.ping(),
        Err(ClientError::Server {
            error: ProtocolError {
                code: ProtocolErrorCode::InvalidRequest,
                ..
            }
        })
    ));
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn request_rejects_mismatched_error_id() {
    let (path, listener) = start_server("mismatched-error");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        send(&mut stream, hello(request.id, PROTOCOL_MAJOR, 0));

        let request = read_request(&mut reader);
        assert!(matches!(request.command, Command::Status));
        send(
            &mut stream,
            ServerMessage::Error {
                id: Some(u64::MAX),
                error: protocol_error(),
            },
        );
    });

    let mut client = Client::connect(&path).unwrap();
    match client.status() {
        Err(ClientError::UnexpectedResponseId { expected, actual }) => {
            assert_ne!(expected, actual);
            assert_eq!(actual, u64::MAX);
        }
        result => panic!("unexpected result: {result:?}"),
    }
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn next_event_surfaces_server_error() {
    let (path, listener) = start_server("event-error");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        send(&mut stream, hello(request.id, PROTOCOL_MAJOR, 0));
        send(
            &mut stream,
            ServerMessage::Error {
                id: Some(u64::MAX),
                error: protocol_error(),
            },
        );
    });

    let mut client = Client::connect(&path).unwrap();
    assert!(matches!(
        client.next_event(),
        Err(ClientError::Server {
            error: ProtocolError {
                code: ProtocolErrorCode::InvalidRequest,
                ..
            }
        })
    ));
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn premature_eof_is_typed() {
    let (path, listener) = start_server("eof");
    let server = thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
    });
    let error = Client::connect(&path).err().unwrap();
    assert!(matches!(error, ClientError::PrematureEof));
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn invalid_frame_is_typed() {
    let (path, listener) = start_server("invalid");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        send(&mut stream, hello(request.id, PROTOCOL_MAJOR, 0));
        let _request = read_request(&mut reader);
        stream.write_all(b"not-json\n").unwrap();
    });

    let mut client = Client::connect(&path).unwrap();
    let error = client.status().err().unwrap();
    assert!(matches!(error, ClientError::Frame(_)));
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn default_socket_requires_absolute_runtime_directory() {
    let _guard = ENV_LOCK.lock().unwrap();
    let old = std::env::var_os("XDG_RUNTIME_DIR");

    unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
    assert!(matches!(
        Client::default_socket_path(),
        Err(ClientError::RuntimeDirectoryMissing)
    ));

    unsafe { std::env::set_var("XDG_RUNTIME_DIR", "relative-runtime") };
    assert!(matches!(
        Client::default_socket_path(),
        Err(ClientError::RuntimeDirectoryRelative(_))
    ));

    unsafe { std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000") };
    assert_eq!(
        Client::default_socket_path().unwrap(),
        PathBuf::from("/run/user/1000/wiiland/wiilandd.sock")
    );

    match old {
        Some(value) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", value) },
        None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
    }
}

#[test]
fn background_capture_preserves_partial_frames_and_cancellation_closes_lease() {
    use std::time::{Duration, Instant};
    use wiiland_ipc::{ButtonEvent, InputPayload, Session, SessionEvent, Timestamp};
    let (path, listener) = start_server("session");
    let (sent, received) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        send(&mut stream, hello(request.id, 1, 1));
        let request = read_request(&mut reader);
        assert!(matches!(request.command, Command::Status));
        send(&mut stream, status(request.id));
        let request = read_request(&mut reader);
        assert!(matches!(request.command, Command::Devices));
        send(
            &mut stream,
            ServerMessage::Response {
                id: request.id,
                result: ResponseResult::Devices(vec![device()]),
            },
        );
        let request = read_request(&mut reader);
        assert!(matches!(request.command, Command::Subscribe { .. }));
        send(
            &mut stream,
            ServerMessage::Response {
                id: request.id,
                result: ResponseResult::Subscribed,
            },
        );
        let request = read_request(&mut reader);
        assert!(
            matches!(request.command, Command::StartCapture { syspath } if syspath == "/sys/test")
        );
        send(
            &mut stream,
            ServerMessage::Response {
                id: request.id,
                result: ResponseResult::CaptureStarted(device()),
            },
        );
        let event = ServerMessage::Notification(Notification::Input {
            sequence: 1,
            syspath: "/sys/test".into(),
            timestamp: Timestamp {
                seconds: 0,
                micros: 0,
            },
            payload: InputPayload::Key(ButtonEvent { code: 4, state: 1 }),
        });
        let bytes = encode_frame(&event).unwrap();
        stream.write_all(&bytes[..5]).unwrap();
        thread::sleep(Duration::from_millis(160));
        stream.write_all(&bytes[5..]).unwrap();
        sent.send(()).unwrap();
        let mut line = Vec::new();
        assert_eq!(
            reader.read_until(b'\n', &mut line).unwrap(),
            0,
            "cancellation must close the capture connection"
        );
    });
    let session = Session::start(Some(path.clone()), "1".into(), true);
    received.recv_timeout(Duration::from_secs(3)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut connected = false;
    let mut input = false;
    while !input {
        match session.try_recv() {
            Ok(SessionEvent::Connected { devices, .. }) => {
                connected = true;
                assert_eq!(devices[0].syspath, "/sys/test");
            }
            Ok(SessionEvent::Notification(Notification::Input {
                payload: InputPayload::Key(button),
                ..
            })) => {
                assert!(connected);
                assert_eq!(button.code, 4);
                input = true;
            }
            Ok(_) => {}
            Err(_) => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
    session.cancel();
    loop {
        if let Ok(result) = session.try_finish() {
            result.unwrap();
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

fn assert_timeout(error: ClientError) {
    assert!(
        matches!(error, ClientError::Io(ref error) if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock)),
        "expected timeout, got {error}"
    );
}

#[test]
fn event_poll_returns_between_fragments_and_retains_the_complete_notification() {
    let (path, listener) = start_server("poll-fragments");
    let (resume, resumed) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        send(
            &mut stream,
            hello(request.id, PROTOCOL_MAJOR, PROTOCOL_MINOR),
        );
        resumed
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let frame = notification_frame(7, 200);
        stream.write_all(&frame[..20]).unwrap();
        resumed
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        stream.write_all(&frame[20..]).unwrap();
    });
    let mut client = Client::connect(&path).unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .unwrap();
    resume.send(()).unwrap();
    assert!(client.poll_event().unwrap().is_none());
    // This is the point where a capture worker can observe cancellation or its
    // deadline, even though the peer has not finished the current frame.
    resume.send(()).unwrap();
    assert!(matches!(
        client.next_event().unwrap(),
        Notification::DeviceRemoved { sequence: 7, .. }
    ));
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn partial_response_bytes_do_not_restart_a_request_deadline() {
    let (path, listener) = start_server("response-deadline");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        send(
            &mut stream,
            hello(request.id, PROTOCOL_MAJOR, PROTOCOL_MINOR),
        );
        let request = read_request(&mut reader);
        for byte in encode_frame(&status(request.id)).unwrap() {
            if stream.write_all(&[byte]).is_err() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
    });
    let mut client = Client::connect(&path).unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_millis(80)))
        .unwrap();
    assert_timeout(client.status().unwrap_err());
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn notifications_do_not_restart_a_correlated_request_deadline() {
    let (path, listener) = start_server("notification-deadline");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        send(
            &mut stream,
            hello(request.id, PROTOCOL_MAJOR, PROTOCOL_MINOR),
        );
        let request = read_request(&mut reader);
        for sequence in 0..32 {
            if stream
                .write_all(&notification_frame(sequence, 200))
                .is_err()
            {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = stream.write_all(&encode_frame(&status(request.id)).unwrap());
    });
    let mut client = Client::connect(&path).unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_millis(80)))
        .unwrap();
    assert_timeout(client.status().unwrap_err());
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn slow_fragmented_hello_observes_the_total_handshake_deadline() {
    let (path, listener) = start_server("hello-deadline");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let request = read_request(&mut reader);
        for byte in encode_frame(&hello(request.id, PROTOCOL_MAJOR, PROTOCOL_MINOR)).unwrap() {
            if stream.write_all(&[byte]).is_err() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(30));
        }
    });
    let error = match Client::connect(&path) {
        Ok(_) => panic!("a trickled Hello must exceed the handshake deadline"),
        Err(error) => error,
    };
    assert_timeout(error);
    server.join().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
