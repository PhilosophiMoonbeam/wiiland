use std::os::unix::process::CommandExt;
use std::process::Command;

const PROGRAM: &str = "wiiland-show";

fn run_as(program: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_wiiland-show"))
        .arg0(program)
        .args(args)
        .output()
        .expect("wiiland-show binary")
}

fn run(args: &[&str]) -> std::process::Output {
    run_as(PROGRAM, args)
}

#[test]
fn help_is_stdout_only_and_lists_every_live_key() {
    let output = run(&["--help"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let help = String::from_utf8_lossy(&output.stdout);
    for command in [
        "q: Quit application",
        "f: Freeze/Unfreeze screen",
        "s: Refresh static values",
        "k: Toggle key events",
        "r: Toggle rumble motor",
        "a: Toggle accelerometer",
        "i: Toggle IR camera",
        "m: Toggle motion plus",
        "n: Toggle normalization",
        "N: Toggle Nunchuk",
        "c: Toggle Classic Controller",
        "b: Toggle balance board",
        "p: Toggle pro controller",
        "g: Toggle guitar controller",
        "d: Toggle drums controller",
        "1-4: Toggle LEDs",
    ] {
        assert!(help.contains(command), "help omitted {command}");
    }
    assert!(help.contains("wiiland-show <positive-ordinal>"));
    assert!(help.contains("wiiland-show /sys/path/to/device"));
}

#[test]
fn missing_and_surplus_selectors_are_strict() {
    for args in [&[][..], &["1", "2"][..]] {
        let output = run(args);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("expected exactly one selector"));
        assert!(error.contains("wiiland-show list"));
    }
}

#[test]
fn help_and_errors_use_the_argv_program_name() {
    const ALTERNATE_PROGRAM: &str = "alternate-wiiland-show";

    let output = run_as(ALTERNATE_PROGRAM, &["--help"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains(&format!("{ALTERNATE_PROGRAM} list")));

    let output = run_as(ALTERNATE_PROGRAM, &[]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.starts_with(&format!("{ALTERNATE_PROGRAM}:")));
    assert!(error.contains(&format!("{ALTERNATE_PROGRAM} list")));
}

#[test]
fn invalid_selectors_never_open_a_device() {
    for selector in ["0", "-1", "+1", "1x", "abc", "/tmp/device", "/sys"] {
        let output = run(&[selector]);
        assert!(!output.status.success(), "accepted selector {selector}");
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("selector") || error.contains("device path"));
    }
}

#[test]
fn list_is_pipeline_safe() {
    let output = run(&["--direct", "list"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split('\t');
        if let Some(number) = fields.next() {
            assert!(number.parse::<usize>().is_ok());
            assert!(fields.next().is_some_and(|path| path.starts_with("/sys/")));
            assert!(fields.next().is_none());
        }
    }
}

#[test]
fn default_list_uses_the_daemon_without_opening_hardware() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use wiiland_ipc::{Request, ResponseResult, ServerMessage};
    let root = std::env::temp_dir().join(format!("wiiland-show-ipc-{}", std::process::id()));
    std::fs::create_dir_all(root.join("wiiland")).unwrap();
    let path = root.join("wiiland/wiilandd.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        for result in [
            ResponseResult::Hello {
                major: 1,
                minor: 1,
                daemon_version: "test".into(),
            },
            ResponseResult::Devices(Vec::new()),
        ] {
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).unwrap();
            let request: Request = wiiland_ipc::decode_frame(&line).unwrap();
            stream
                .write_all(
                    &wiiland_ipc::encode_frame(&ServerMessage::Response {
                        id: request.id,
                        result,
                    })
                    .unwrap(),
                )
                .unwrap();
        }
    });
    let output = Command::new(env!("CARGO_BIN_EXE_wiiland-show"))
        .env("XDG_RUNTIME_DIR", &root)
        .arg("list")
        .output()
        .unwrap();
    server.join().unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}
