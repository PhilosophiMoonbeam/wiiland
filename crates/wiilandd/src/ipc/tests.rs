use super::*;
use std::cell::Cell;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;
use tempfile::tempdir;
use wiiland_ipc::{
    ButtonEvent, InputPayload, Profile, ResponseResult, Timestamp, decode_frame, encode_frame,
};
use wiiland_ipc::{PROTOCOL_MAJOR, Request, Subscription};

fn private_socket_path(root: &Path, directory: &str) -> PathBuf {
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    let parent = root.join(directory);
    assert!(!parent.exists());
    parent.join("wiilandd.sock")
}

fn status(path: &Path) -> Status {
    Status {
        daemon_version: "test".into(),
        pid: 7,
        device_count: 1,
        dry_run: true,
        socket_path: path.display().to_string(),
    }
}

fn device() -> DeviceInfo {
    DeviceInfo {
        syspath: "/sys/test".into(),
        profile: Profile::Gamepad,
        opened_interfaces: 1,
        pending_interfaces: 0,
        gamepad_output: true,
        desktop_output: false,
    }
}

fn ready(
    server: &mut IpcServer,
    token: u64,
    revents: i16,
    st: &Status,
    device_snapshot: &[DeviceInfo],
) -> io::Result<()> {
    let mut status_provider = |_: &Path| st.clone();
    let mut devices_provider = || device_snapshot.to_vec();
    server.handle_ready(token, revents, &mut status_provider, &mut devices_provider)
}

fn input_notification_with_frame_len(sequence: u64, frame_len: usize) -> Notification {
    let make = |syspath: String| Notification::Input {
        sequence,
        syspath,
        timestamp: Timestamp {
            seconds: 0,
            micros: 0,
        },
        payload: InputPayload::Key(ButtonEvent { code: 1, state: 1 }),
    };
    let base = make(String::new());
    let base_len = encode_frame(&ServerMessage::Notification(base))
        .unwrap()
        .len();
    assert!(frame_len >= base_len);
    let notification = make("x".repeat(frame_len - base_len));
    assert_eq!(
        encode_frame(&ServerMessage::Notification(notification.clone()))
            .unwrap()
            .len(),
        frame_len
    );
    notification
}

fn connect(server: &mut IpcServer) -> (UnixStream, u64) {
    let mut sources = Vec::new();
    server.poll_sources(&mut sources);
    let existing_tokens: Vec<_> = sources
        .iter()
        .filter(|source| source.token != LISTENER_TOKEN)
        .map(|source| source.token)
        .collect();

    let stream = UnixStream::connect(server.path()).unwrap();
    let path = server.path().to_path_buf();
    let st = status(&path);
    ready(server, LISTENER_TOKEN, libc::POLLIN, &st, &[]).unwrap();
    server.poll_sources(&mut sources);
    let mut new_tokens = sources
        .iter()
        .filter(|source| source.token != LISTENER_TOKEN && !existing_tokens.contains(&source.token))
        .map(|source| source.token);
    let token = new_tokens.next().expect("accepted client token");
    assert_eq!(
        new_tokens.next(),
        None,
        "accepted client token must be unique"
    );
    (stream, token)
}

fn read_message(stream: &mut UnixStream) -> ServerMessage {
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0; 1];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            return decode_frame(&bytes).unwrap();
        }
    }
}

fn request(
    server: &mut IpcServer,
    stream: &mut UnixStream,
    token: u64,
    id: u64,
    command: Command,
) -> ServerMessage {
    stream
        .write_all(&encode_frame(&Request { id, command }).unwrap())
        .unwrap();
    let st = status(server.path());
    ready(server, token, libc::POLLIN, &st, &[device()]).unwrap();
    ready(server, token, libc::POLLOUT, &st, &[device()]).unwrap();
    read_message(stream)
}

#[test]
fn capture_leases_are_per_connection_and_removed_on_stop_or_disconnect() {
    let dir = tempdir().unwrap();
    let path = private_socket_path(dir.path(), "lease");
    let mut server = IpcServer::bind(path).unwrap();
    let (stream_a, a) = connect(&mut server);
    let (stream_b, b) = connect(&mut server);
    server.complete_command(a, 1, Ok(ResponseResult::CaptureStarted(device())));
    server.complete_command(a, 2, Ok(ResponseResult::CaptureStarted(device())));
    server.complete_command(b, 1, Ok(ResponseResult::CaptureStarted(device())));
    assert_eq!(server.capture_paths().len(), 2);
    server.complete_command(a, 3, Ok(ResponseResult::CaptureStopped));
    assert_eq!(server.capture_paths(), [device().syspath]);
    drop(stream_b);
    let st = status(server.path());
    ready(&mut server, b, libc::POLLIN | libc::POLLHUP, &st, &[]).unwrap();
    assert!(server.capture_paths().is_empty());
    drop(stream_a);
}

#[test]
fn deferred_controls_preserve_request_identity_and_bound_pending_work() {
    let (stream, _peer) = UnixStream::pair().unwrap();
    let mut client = Client::new(stream);
    client.negotiated = true;
    let mut status = |path: &Path| status(path);
    let mut devices = || vec![device()];
    for id in 1..=FRAME_BUDGET as u64 {
        let frame = encode_frame(&Request {
            id,
            command: Command::Diagnostics,
        })
        .unwrap();
        assert!(handle_frame(
            &mut client,
            &frame,
            Path::new("/tmp/socket"),
            &mut status,
            &mut devices
        ));
    }
    assert_eq!(client.commands.front().unwrap().0, 1);
    assert_eq!(client.commands.len(), FRAME_BUDGET);
    let frame = encode_frame(&Request {
        id: 99,
        command: Command::Config,
    })
    .unwrap();
    handle_frame(
        &mut client,
        &frame,
        Path::new("/tmp/socket"),
        &mut status,
        &mut devices,
    );
    assert!(client.immediate_close);
    assert_eq!(client.commands.len(), FRAME_BUDGET);
}

#[test]
fn secure_bind_modes_live_collision_stale_and_cleanup() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        IpcServer::bind(dir.path().join("nonprivate.sock"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();

    let path = private_socket_path(dir.path(), "private");
    let server = IpcServer::bind(&path).unwrap();
    assert_eq!(
        fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        IpcServer::bind(&path).unwrap_err().kind(),
        io::ErrorKind::AddrInUse
    );
    drop(server);
    assert!(!path.exists());

    let replacement = UnixListener::bind(&path).unwrap();
    assert_eq!(
        IpcServer::bind(&path).unwrap_err().kind(),
        io::ErrorKind::AddrInUse
    );
    drop(replacement);
    assert!(path.exists());

    let server = IpcServer::bind(&path).unwrap();
    drop(server);
    assert!(!path.exists());
}

#[test]
fn drop_cleanup_restores_and_never_unlinks_a_replacement_entry() {
    let dir = tempdir().unwrap();
    let path = private_socket_path(dir.path(), "private");
    let mut server = IpcServer::bind(&path).unwrap();
    let moved = path.with_file_name("moved.sock");
    let replacement = Arc::new(Mutex::new(None::<UnixListener>));
    let replacement_for_hook = Arc::clone(&replacement);
    let path_for_hook = path.clone();
    let moved_for_hook = moved.clone();
    server.before_drop_cleanup = Some(Box::new(move || {
        fs::rename(&path_for_hook, &moved_for_hook).unwrap();
        let listener = UnixListener::bind(&path_for_hook).unwrap();
        *replacement_for_hook.lock().unwrap() = Some(listener);
    }));

    drop(server);
    assert!(path.exists());
    assert!(moved.exists());
    drop(replacement.lock().unwrap().take());
    fs::remove_file(&path).unwrap();
    fs::remove_file(&moved).unwrap();
}

#[test]
fn stale_cleanup_restores_and_never_unlinks_a_replacement_entry() {
    let dir = tempdir().unwrap();
    let path = private_socket_path(dir.path(), "private");
    fs::create_dir(path.parent().unwrap()).unwrap();
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    drop(UnixListener::bind(&path).unwrap());
    let moved = path.with_file_name("observed-stale.sock");
    let replacement = Arc::new(Mutex::new(None::<UnixListener>));
    let replacement_for_hook = Arc::clone(&replacement);
    let path_for_hook = path.clone();
    let moved_for_hook = moved.clone();

    let error = IpcServer::bind_with_hooks(
        &path,
        |_, _, _| Ok(()),
        move || {
            fs::rename(&path_for_hook, &moved_for_hook).unwrap();
            let listener = UnixListener::bind(&path_for_hook).unwrap();
            *replacement_for_hook.lock().unwrap() = Some(listener);
        },
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    assert!(path.exists());
    assert!(moved.exists());
    drop(replacement.lock().unwrap().take());
    fs::remove_file(&path).unwrap();
    fs::remove_file(&moved).unwrap();
}

#[test]
fn parent_replacement_before_bind_success_is_rejected() {
    let dir = tempdir().unwrap();
    let path = private_socket_path(dir.path(), "private");
    let parent = path.parent().unwrap().to_path_buf();
    let moved_parent = dir.path().join("pinned-parent");
    let parent_for_hook = parent.clone();
    let moved_for_hook = moved_parent.clone();

    let error = IpcServer::bind_with_setup(&path, move |_, _, _| {
        fs::rename(&parent_for_hook, &moved_for_hook)?;
        fs::create_dir(&parent_for_hook)?;
        fs::set_permissions(&parent_for_hook, fs::Permissions::from_mode(0o700))
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    assert!(!path.exists());
    assert!(!moved_parent.join("wiilandd.sock").exists());
    assert!(moved_parent.join("wiilandd.sock.lock").exists());
}
#[test]
fn missing_parent_requires_private_grandparent_without_side_effects() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let path = dir.path().join("private").join("wiilandd.sock");

    assert_eq!(
        IpcServer::bind(&path).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(!path.parent().unwrap().exists());
    assert!(!socket_lock_path(&path).unwrap().exists());
}

#[test]
fn symlink_parent_and_lock_are_rejected_without_chmod_following() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let real_parent = dir.path().join("real");
    fs::create_dir(&real_parent).unwrap();
    fs::set_permissions(&real_parent, fs::Permissions::from_mode(0o700)).unwrap();
    let linked_parent = dir.path().join("linked");
    symlink(&real_parent, &linked_parent).unwrap();
    let linked_path = linked_parent.join("wiilandd.sock");
    assert_eq!(
        IpcServer::bind(&linked_path).unwrap_err().kind(),
        io::ErrorKind::NotADirectory
    );

    let path = real_parent.join("wiilandd.sock");
    let victim = real_parent.join("victim");
    fs::write(&victim, b"not a lock").unwrap();
    fs::set_permissions(&victim, fs::Permissions::from_mode(0o640)).unwrap();
    symlink(&victim, socket_lock_path(&path).unwrap()).unwrap();
    assert!(IpcServer::bind(&path).is_err());
    assert_eq!(
        fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert!(!path.exists());
}

#[test]
fn setup_never_chmods_a_replacement_symlink_target() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let path = private_socket_path(dir.path(), "private");
    let moved = path.with_file_name("bound.sock");
    let victim = dir.path().join("victim");
    fs::write(&victim, b"victim").unwrap();
    fs::set_permissions(&victim, fs::Permissions::from_mode(0o640)).unwrap();

    let error = IpcServer::bind_with_setup(&path, |path, _, _| {
        fs::rename(path, &moved)?;
        symlink(&victim, path)
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    assert_eq!(
        fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert!(
        fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    fs::remove_file(&path).unwrap();
    fs::remove_file(&moved).unwrap();
}

#[test]
fn startup_lock_serializes_concurrent_bind_and_persists_after_drop() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("private").join("wiilandd.sock");
    let barrier = Arc::new(Barrier::new(3));
    let mut joins = Vec::new();
    for _ in 0..2 {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        joins.push(thread::spawn(move || {
            barrier.wait();
            IpcServer::bind(path)
        }));
    }
    barrier.wait();
    let mut winner = None;
    let mut loser = None;
    for join in joins {
        match join.join().unwrap() {
            Ok(server) => winner = Some(server),
            Err(error) => loser = Some(error),
        }
    }
    assert!(winner.is_some());
    assert_eq!(loser.unwrap().kind(), io::ErrorKind::AddrInUse);
    assert!(path.exists());
    let lock_path = socket_lock_path(&path).unwrap();
    assert_eq!(
        fs::symlink_metadata(&lock_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    drop(winner);
    assert!(!path.exists());
    assert!(
        fs::symlink_metadata(&lock_path)
            .unwrap()
            .file_type()
            .is_file()
    );
    let replacement = IpcServer::bind(&path).unwrap();
    drop(replacement);
    assert!(lock_path.exists());
}

#[test]
fn failed_post_bind_setup_removes_created_socket() {
    let dir = tempdir().unwrap();
    let path = private_socket_path(dir.path(), "private");
    let error = IpcServer::bind_with_setup(&path, |_, _, _| {
        Err(io::Error::other("injected setup failure"))
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert!(!path.exists());
}

#[test]
fn failed_post_bind_setup_preserves_replacement_inode() {
    let dir = tempdir().unwrap();
    let path = private_socket_path(dir.path(), "private");
    let moved = path.with_file_name("created.sock");
    let error = IpcServer::bind_with_setup(&path, |path, _, _| {
        fs::rename(path, &moved)?;
        let replacement = UnixListener::bind(path)?;
        drop(replacement);
        Err(io::Error::other("injected setup failure"))
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert!(path.exists());
    fs::remove_file(path).unwrap();
    fs::remove_file(moved).unwrap();
}

#[test]
fn hello_errors_are_correlated_and_status_devices_work() {
    let dir = tempdir().unwrap();
    let path = private_socket_path(dir.path(), "initial");
    let mut server = IpcServer::bind(&path).unwrap();
    let (mut stream, token) = connect(&mut server);
    let error = request(&mut server, &mut stream, token, 9, Command::Ping);
    assert!(matches!(error, ServerMessage::Error { id: Some(9), .. }));
    let path = private_socket_path(dir.path(), "unsupported");
    let mut server = IpcServer::bind(&path).unwrap();
    let (mut stream, token) = connect(&mut server);
    let error = request(
        &mut server,
        &mut stream,
        token,
        8,
        Command::Hello {
            min_major: 9,
            max_major: 9,
        },
    );
    assert!(matches!(
        error,
        ServerMessage::Error {
            id: Some(8),
            error: ProtocolError {
                code: ProtocolErrorCode::UnsupportedVersion,
                ..
            }
        }
    ));

    let path = private_socket_path(dir.path(), "successful");
    let mut server = IpcServer::bind(&path).unwrap();
    let (mut stream, token) = connect(&mut server);
    let hello = request(
        &mut server,
        &mut stream,
        token,
        1,
        Command::Hello {
            min_major: 1,
            max_major: 1,
        },
    );
    assert!(matches!(
        hello,
        ServerMessage::Response {
            result: ResponseResult::Hello { .. },
            ..
        }
    ));
    let status_message = request(&mut server, &mut stream, token, 2, Command::Status);
    assert!(matches!(
        status_message,
        ServerMessage::Response {
            result: ResponseResult::Status(_),
            ..
        }
    ));
    let devices_message = request(&mut server, &mut stream, token, 3, Command::Devices);
    assert!(matches!(
        devices_message,
        ServerMessage::Response {
            result: ResponseResult::Devices(_),
            ..
        }
    ));
}

#[test]
fn snapshot_providers_are_invoked_only_for_their_commands() {
    let dir = tempdir().unwrap();
    let path = private_socket_path(dir.path(), "lazy");
    let mut server = IpcServer::bind(&path).unwrap();
    let expected_status = Status {
        daemon_version: "lazy-status".into(),
        pid: 99,
        device_count: 1,
        dry_run: false,
        socket_path: path.display().to_string(),
    };
    let mut expected_device = device();
    expected_device.syspath = "/sys/lazy-device".into();
    let expected_devices = vec![expected_device];
    let status_calls = Cell::new(0);
    let devices_calls = Cell::new(0);
    let mut status_provider = |socket_path: &Path| {
        assert_eq!(socket_path, path);
        status_calls.set(status_calls.get() + 1);
        expected_status.clone()
    };
    let mut devices_provider = || {
        devices_calls.set(devices_calls.get() + 1);
        expected_devices.clone()
    };

    let mut stream = UnixStream::connect(&path).unwrap();
    server
        .handle_ready(
            LISTENER_TOKEN,
            libc::POLLIN,
            &mut status_provider,
            &mut devices_provider,
        )
        .unwrap();
    assert_eq!((status_calls.get(), devices_calls.get()), (0, 0));
    let mut sources = Vec::new();
    server.poll_sources(&mut sources);
    let token = sources
        .iter()
        .find(|source| source.token != LISTENER_TOKEN)
        .unwrap()
        .token;

    for (id, command) in [
        (
            1,
            Command::Hello {
                min_major: PROTOCOL_MAJOR,
                max_major: PROTOCOL_MAJOR,
            },
        ),
        (2, Command::Ping),
        (
            3,
            Command::Subscribe {
                subscriptions: vec![Subscription::Input],
            },
        ),
    ] {
        stream
            .write_all(&encode_frame(&Request { id, command }).unwrap())
            .unwrap();
        server
            .handle_ready(
                token,
                libc::POLLIN,
                &mut status_provider,
                &mut devices_provider,
            )
            .unwrap();
        let _ = read_message(&mut stream);
        assert_eq!((status_calls.get(), devices_calls.get()), (0, 0));
    }

    server.publish(Notification::Input {
        sequence: 1,
        syspath: "/sys/lazy-device".into(),
        timestamp: Timestamp {
            seconds: 1,
            micros: 2,
        },
        payload: InputPayload::Key(ButtonEvent { code: 1, state: 1 }),
    });
    server
        .handle_ready(
            token,
            libc::POLLOUT,
            &mut status_provider,
            &mut devices_provider,
        )
        .unwrap();
    assert!(matches!(
        read_message(&mut stream),
        ServerMessage::Notification(Notification::Input { .. })
    ));
    assert_eq!((status_calls.get(), devices_calls.get()), (0, 0));

    stream
        .write_all(
            &encode_frame(&Request {
                id: 4,
                command: Command::Status,
            })
            .unwrap(),
        )
        .unwrap();
    server
        .handle_ready(
            token,
            libc::POLLIN,
            &mut status_provider,
            &mut devices_provider,
        )
        .unwrap();
    assert_eq!(
        read_message(&mut stream),
        ServerMessage::Response {
            id: 4,
            result: ResponseResult::Status(expected_status.clone()),
        }
    );
    assert_eq!((status_calls.get(), devices_calls.get()), (1, 0));

    stream
        .write_all(
            &encode_frame(&Request {
                id: 5,
                command: Command::Devices,
            })
            .unwrap(),
        )
        .unwrap();
    server
        .handle_ready(
            token,
            libc::POLLIN,
            &mut status_provider,
            &mut devices_provider,
        )
        .unwrap();
    assert_eq!(
        read_message(&mut stream),
        ServerMessage::Response {
            id: 5,
            result: ResponseResult::Devices(expected_devices.clone()),
        }
    );
    assert_eq!((status_calls.get(), devices_calls.get()), (1, 1));

    drop(stream);
    server
        .handle_ready(
            token,
            libc::POLLHUP,
            &mut status_provider,
            &mut devices_provider,
        )
        .unwrap();
    assert_eq!((status_calls.get(), devices_calls.get()), (1, 1));
    assert!(!server.clients.contains_key(&token));
}

#[test]
fn partial_multiple_frames_and_subscription_filtering() {
    let dir = tempdir().unwrap();
    let path = private_socket_path(dir.path(), "private");
    let mut server = IpcServer::bind(&path).unwrap();
    let (mut stream, token) = connect(&mut server);
    assert!(!server.has_input_subscribers());
    let hello = encode_frame(&Request {
        id: 1,
        command: Command::Hello {
            min_major: 1,
            max_major: 1,
        },
    })
    .unwrap();
    let ping = encode_frame(&Request {
        id: 2,
        command: Command::Ping,
    })
    .unwrap();
    stream.write_all(&hello[..hello.len() / 2]).unwrap();
    let st = status(&path);
    ready(&mut server, token, libc::POLLIN, &st, &[]).unwrap();
    stream
        .write_all(&[hello[hello.len() / 2..].as_ref(), ping.as_ref()].concat())
        .unwrap();
    ready(&mut server, token, libc::POLLIN, &st, &[]).unwrap();
    ready(&mut server, token, libc::POLLOUT, &st, &[]).unwrap();
    assert!(matches!(
        read_message(&mut stream),
        ServerMessage::Response {
            result: ResponseResult::Hello { .. },
            ..
        }
    ));
    assert!(matches!(
        read_message(&mut stream),
        ServerMessage::Response {
            result: ResponseResult::Pong,
            ..
        }
    ));

    let _ = request(
        &mut server,
        &mut stream,
        token,
        3,
        Command::Subscribe {
            subscriptions: vec![Subscription::Input],
        },
    );
    assert!(server.has_input_subscribers());
    server.publish(Notification::DeviceAdded {
        sequence: 1,
        device: device(),
    });
    server.publish(Notification::Input {
        sequence: 2,
        syspath: "/sys/test".into(),
        timestamp: Timestamp {
            seconds: 1,
            micros: 2,
        },
        payload: InputPayload::Key(ButtonEvent { code: 1, state: 1 }),
    });
    ready(&mut server, token, libc::POLLOUT, &st, &[]).unwrap();
    let notification = read_message(&mut stream);
    assert!(matches!(
        notification,
        ServerMessage::Notification(Notification::Input { sequence: 2, .. })
    ));
}

#[test]
fn buffered_frames_are_rescheduled_with_a_bounded_frame_budget() {
    let dir = tempdir().unwrap();
    let path = private_socket_path(dir.path(), "private");
    let mut server = IpcServer::bind(&path).unwrap();
    let (mut stream, token) = connect(&mut server);
    let request_count = FRAME_BUDGET * 3 + 1;
    let mut batch = Vec::new();
    for id in 1..=request_count as u64 {
        let command = if id == 1 {
            Command::Hello {
                min_major: PROTOCOL_MAJOR,
                max_major: PROTOCOL_MAJOR,
            }
        } else {
            Command::Ping
        };
        batch.extend(encode_frame(&Request { id, command }).unwrap());
    }
    stream.write_all(&batch).unwrap();

    let st = status(&path);
    ready(&mut server, token, libc::POLLIN, &st, &[]).unwrap();
    let client = server.clients.get(&token).unwrap();
    assert_eq!(client.pending_frames.len(), request_count - FRAME_BUDGET);
    assert!(client.output.is_empty());

    let max_iterations = request_count.div_ceil(FRAME_BUDGET);
    let mut iterations = 1;
    while !server.clients[&token].pending_frames.is_empty() {
        assert!(iterations < max_iterations);
        let mut sources = Vec::new();
        server.poll_sources(&mut sources);
        let source = sources.iter().find(|source| source.token == token).unwrap();
        assert_ne!(source.events & libc::POLLOUT, 0);
        ready(&mut server, token, libc::POLLOUT, &st, &[]).unwrap();
        iterations += 1;
    }
    assert_eq!(iterations, max_iterations);

    for expected_id in 1..=request_count as u64 {
        match read_message(&mut stream) {
            ServerMessage::Response { id, result } => {
                assert_eq!(id, expected_id);
                if expected_id == 1 {
                    assert!(matches!(result, ResponseResult::Hello { .. }));
                } else {
                    assert_eq!(result, ResponseResult::Pong);
                }
            }
            message => panic!("unexpected response: {message:?}"),
        }
    }
}

#[test]
fn oversized_client_isolated_without_blocking_the_test_writer() {
    let dir = tempdir().unwrap();
    let path = private_socket_path(dir.path(), "private");
    let mut server = IpcServer::bind(&path).unwrap();
    let (mut oversized, oversized_token) = connect(&mut server);
    let (mut healthy, healthy_token) = connect(&mut server);
    let st = status(&path);
    let bytes = vec![b'x'; wiiland_ipc::MAX_FRAME_BYTES + 1];
    for chunk in bytes.chunks(4 * 1024) {
        oversized.write_all(chunk).unwrap();
        ready(&mut server, oversized_token, libc::POLLIN, &st, &[]).unwrap();
    }
    let mut sources = Vec::new();
    server.poll_sources(&mut sources);
    assert!(!sources.iter().any(|source| source.token == oversized_token));

    let hello = encode_frame(&Request {
        id: 1,
        command: Command::Hello {
            min_major: PROTOCOL_MAJOR,
            max_major: PROTOCOL_MAJOR,
        },
    })
    .unwrap();
    healthy.write_all(&hello).unwrap();
    ready(&mut server, healthy_token, libc::POLLIN, &st, &[]).unwrap();
    assert!(matches!(
        read_message(&mut healthy),
        ServerMessage::Response {
            result: ResponseResult::Hello { .. },
            ..
        }
    ));
}

#[test]
fn oversized_response_returns_correlated_internal_error_and_connection_survives() {
    let dir = tempdir().unwrap();
    let path = private_socket_path(dir.path(), "private");
    let mut server = IpcServer::bind(&path).unwrap();
    let (mut stream, token) = connect(&mut server);
    let _ = request(
        &mut server,
        &mut stream,
        token,
        1,
        Command::Hello {
            min_major: PROTOCOL_MAJOR,
            max_major: PROTOCOL_MAJOR,
        },
    );

    let mut oversized = device();
    oversized.syspath = "x".repeat(wiiland_ipc::MAX_FRAME_BYTES);
    stream
        .write_all(
            &encode_frame(&Request {
                id: 2,
                command: Command::Devices,
            })
            .unwrap(),
        )
        .unwrap();
    let st = status(&path);
    ready(&mut server, token, libc::POLLIN, &st, &[oversized]).unwrap();
    assert!(matches!(
        read_message(&mut stream),
        ServerMessage::Error {
            id: Some(2),
            error: ProtocolError {
                code: ProtocolErrorCode::Internal,
                ..
            }
        }
    ));
    assert!(server.clients.contains_key(&token));
    assert!(matches!(
        request(&mut server, &mut stream, token, 3, Command::Ping),
        ServerMessage::Response {
            id: 3,
            result: ResponseResult::Pong
        }
    ));
}

#[test]
fn protocol_error_is_flushed_before_client_close() {
    let dir = tempdir().unwrap();
    let path = private_socket_path(dir.path(), "private");
    let mut server = IpcServer::bind(&path).unwrap();
    let (mut stream, token) = connect(&mut server);
    let invalid_first_request = encode_frame(&Request {
        id: 7,
        command: Command::Ping,
    })
    .unwrap();
    let st = status(&path);
    let mut status_provider = |_: &Path| st.clone();
    let mut devices_provider = || Vec::new();
    assert!(handle_frame(
        server.clients.get_mut(&token).unwrap(),
        &invalid_first_request,
        &path,
        &mut status_provider,
        &mut devices_provider,
    ));
    assert!(server.clients[&token].closing);
    let mut sources = Vec::new();
    server.poll_sources(&mut sources);
    let source = sources.iter().find(|source| source.token == token).unwrap();
    assert_eq!(source.events & libc::POLLIN, 0);
    assert_ne!(source.events & libc::POLLOUT, 0);

    server.publish(Notification::DeviceAdded {
        sequence: 1,
        device: device(),
    });
    assert!(server.clients.contains_key(&token));
    ready(&mut server, token, libc::POLLOUT, &st, &[]).unwrap();
    assert!(!server.clients.contains_key(&token));
    assert!(matches!(
        read_message(&mut stream),
        ServerMessage::Error {
            id: Some(7),
            error: ProtocolError {
                code: ProtocolErrorCode::InvalidRequest,
                ..
            }
        }
    ));
}

#[test]
fn response_queue_overflow_removes_client_without_flushing() {
    let dir = tempdir().unwrap();
    let path = private_socket_path(dir.path(), "private");
    let mut server = IpcServer::bind(&path).unwrap();
    let (mut stream, token) = connect(&mut server);
    let _ = request(
        &mut server,
        &mut stream,
        token,
        1,
        Command::Hello {
            min_major: PROTOCOL_MAJOR,
            max_major: PROTOCOL_MAJOR,
        },
    );

    let client = server.clients.get_mut(&token).unwrap();
    assert!(client.queue_encoded(vec![b'x'; MAX_QUEUED_BYTES]));
    assert!(!client.queue_encoded(vec![b'y']));
    ready(
        &mut server,
        token,
        libc::POLLIN | libc::POLLOUT,
        &status(&path),
        &[],
    )
    .unwrap();
    assert!(!server.clients.contains_key(&token));
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut byte = [0];
    assert_eq!(stream.read(&mut byte).unwrap(), 0);
}

#[test]
fn queued_byte_limit_keeps_boundary_and_evicts_first_crossing_frame() {
    let dir = tempdir().unwrap();
    let path = private_socket_path(dir.path(), "private");
    let mut server = IpcServer::bind(&path).unwrap();
    let (mut stream, token) = connect(&mut server);
    let _ = request(
        &mut server,
        &mut stream,
        token,
        1,
        Command::Hello {
            min_major: PROTOCOL_MAJOR,
            max_major: PROTOCOL_MAJOR,
        },
    );
    let _ = request(
        &mut server,
        &mut stream,
        token,
        2,
        Command::Subscribe {
            subscriptions: vec![Subscription::Input],
        },
    );

    const LARGE_FRAME: usize = 48 * 1024;
    for sequence in 0..5 {
        server.publish(input_notification_with_frame_len(sequence, LARGE_FRAME));
        assert!(server.clients.contains_key(&token));
    }
    let remainder = MAX_QUEUED_BYTES - 5 * LARGE_FRAME;
    server.publish(input_notification_with_frame_len(5, remainder));
    assert_eq!(server.clients[&token].queued_bytes, MAX_QUEUED_BYTES);

    let mut sources = Vec::new();
    server.poll_sources(&mut sources);
    assert!(sources.iter().any(|source| source.token == token));

    let crossing = input_notification_with_frame_len(6, 1024);
    server.publish(crossing);
    server.poll_sources(&mut sources);
    assert!(!sources.iter().any(|source| source.token == token));
}
