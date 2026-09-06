use wiilandd::signal::SignalPipe;
use wiilandd::uinput::{RecordingBackend, RecordingOp, VirtualDevice, VirtualKind};

#[test]
fn uinput_recording_backend_preserves_identity_setup_and_cleanup_order() {
    let backend = RecordingBackend::new();
    let view = backend.clone();
    {
        let device = VirtualDevice::with_backend("/dev/uinput", VirtualKind::Desktop, backend)
            .expect("recording backend opens");
        assert_eq!(device.kind(), VirtualKind::Desktop);
        assert!(device.fd() >= 0);
    }
    let ops = view.operations();
    let create = ops
        .iter()
        .position(|op| {
            matches!(
                op,
                RecordingOp::Ioctl {
                    request: 0x5501,
                    ..
                }
            )
        })
        .expect("device create");
    let destroy = ops
        .iter()
        .position(|op| matches!(op, RecordingOp::Destroy))
        .expect("device destroy");
    let close = ops
        .iter()
        .position(|op| matches!(op, RecordingOp::Close))
        .expect("fd close");
    assert!(create < destroy && destroy < close);
}

#[test]
fn signal_pipe_teardown_disarms_handler_before_fd_close() {
    let pipe = SignalPipe::install().expect("self-pipe");
    assert!(matches!(SignalPipe::install(), Err(code) if code == -libc::EBUSY));
    assert!(!pipe.requested());
    let (read, write) = pipe.fds();
    assert!(read >= 0 && write >= 0 && read != write);
    drop(pipe);
    let replacement = SignalPipe::install().expect("replacement self-pipe");
    assert!(!replacement.requested());
    drop(replacement);
}

#[test]
fn recording_backend_short_write_is_reported_as_eio() {
    let mut backend = RecordingBackend::new();
    backend.short_write = Some(1);
    let view = backend.clone();
    let error = match VirtualDevice::with_backend("/dev/uinput", VirtualKind::Controller, backend) {
        Ok(_) => panic!("short setup write must fail"),
        Err(error) => error,
    };
    assert_eq!(error, -libc::EIO);
    assert_eq!(
        view.operations()
            .iter()
            .filter(|op| matches!(op, RecordingOp::Close))
            .count(),
        1
    );
    assert!(
        !view
            .operations()
            .iter()
            .any(|op| matches!(op, RecordingOp::Destroy))
    );
}

#[test]
fn failed_uinput_setup_and_creation_close_exactly_once() {
    for request in [0x4004_5564, 0x5501] {
        let mut backend = RecordingBackend::new();
        backend.fail_ioctl = Some(request);
        let view = backend.clone();
        assert!(
            VirtualDevice::with_backend("/dev/uinput", VirtualKind::Controller, backend).is_err()
        );
        let ops = view.operations();
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op, RecordingOp::Close))
                .count(),
            1
        );
        assert!(!ops.iter().any(|op| matches!(op, RecordingOp::Destroy)));
    }
}
