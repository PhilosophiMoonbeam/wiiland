#![cfg(target_os = "linux")]

use std::os::fd::BorrowedFd;
use std::path::PathBuf;

use wiiland_hid::{Monitor, MonitorMode};

#[test]
fn enumeration_has_a_single_boundary_and_stays_exhausted() {
    let Ok(mut monitor) = Monitor::new(MonitorMode::Enumerate) else {
        return;
    };
    let fd: Option<BorrowedFd<'_>> = monitor.fd();
    assert!(fd.is_none());

    // Enumeration is finite even when the host happens to expose several HID
    // devices, and each device is observed at most once in the initial snapshot.
    let mut initial: Vec<PathBuf> = Vec::new();
    while let Some(path) = monitor.poll().expect("udev enumeration") {
        assert!(path.is_absolute());
        initial.push(path);
        assert!(
            initial.len() < 4096,
            "udev enumeration did not reach its boundary"
        );
    }
    assert!(
        monitor
            .poll()
            .expect("enumeration remains healthy")
            .is_none()
    );
    assert!(
        monitor
            .poll()
            .expect("enumeration remains healthy")
            .is_none()
    );
    assert!(initial.windows(2).all(|pair| pair[0] != pair[1]));
}

#[test]
fn watch_mode_uses_a_borrowed_udev_fd_and_reports_would_block_as_none() {
    let Ok(mut monitor) = Monitor::new(MonitorMode::Watch) else {
        return;
    };
    assert!(monitor.fd().is_some());

    // A watch monitor is non-blocking at the API boundary: absence of a
    // currently queued udev event is represented by Ok(None).
    match monitor.poll() {
        Ok(Some(path)) => assert!(path.is_absolute()),
        Ok(None) => {}
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            panic!("watch poll leaked WouldBlock instead of returning Ok(None)")
        }
        Err(error) => panic!("unexpected udev watch failure: {error}"),
    }
}
