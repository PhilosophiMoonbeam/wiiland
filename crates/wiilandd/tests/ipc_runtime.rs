use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use wiiland_ipc::{Client, ClientError, ProtocolErrorCode};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn real_reactor_serves_control_requests_and_shuts_down_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("wiilandd.sock");
    let mut daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_wiilandd"))
            .args(["--no-config", "--dry-run", "--ipc-socket"])
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut client = loop {
        match Client::connect(&socket) {
            Ok(client) => break client,
            Err(error) => {
                assert!(
                    daemon.0.try_wait().unwrap().is_none(),
                    "daemon exited: {error}"
                );
                assert!(Instant::now() < deadline, "daemon failed to start: {error}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    client
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    assert!(client.status().unwrap().dry_run);
    let config = client.config().unwrap();
    assert_eq!(
        wiiland_core::Config::parse_bytes("running", config.as_bytes()).unwrap(),
        wiiland_core::Config::default()
    );
    let metrics = client.diagnostics().unwrap();
    assert_eq!(metrics.trace_records_dropped, 0);
    let error = client
        .start_capture("/sys/wiiland-nonexistent-test-device")
        .unwrap_err();
    assert!(
        matches!(error, ClientError::Server { error } if error.code == ProtocolErrorCode::InvalidRequest)
    );
    client.stop_capture().unwrap();
    client.ping().unwrap();
    // Signal handling, control requests and cleanup run in the actual reactor.
    assert_eq!(
        unsafe { libc::kill(daemon.0.id() as i32, libc::SIGTERM) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = daemon.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "shutdown stalled");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!socket.exists());
}
