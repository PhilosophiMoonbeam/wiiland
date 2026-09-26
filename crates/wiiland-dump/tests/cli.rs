use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn invoke(arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_wiiland-dump"))
        .args(arguments)
        .output()
        .expect("run wiiland-dump")
}

fn temporary_file(label: &str, contents: &[u8]) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "wiiland-dump-cli-{label}-{}-{timestamp}-{id}",
        std::process::id()
    ));
    fs::write(&path, contents).expect("write EEPROM fixture");
    path
}

fn remove(path: PathBuf) {
    fs::remove_file(path).expect("remove EEPROM fixture");
}

#[test]
fn help_is_stdout_only_and_exact() {
    let output = invoke(&["--help"]);
    assert!(output.status.success());
    assert_eq!(output.stderr, b"");
    let program = env!("CARGO_BIN_EXE_wiiland-dump");
    let expected = format!(
        "Usage: {program} FILE\nRead a Wii Remote EEPROM file and write its contents to stdout.\n"
    );
    assert_eq!(output.stdout, expected.as_bytes());
}

#[test]
fn arity_errors_use_stderr() {
    let cases: [&[&str]; 2] = [&[], &["one", "two"]];
    for arguments in cases {
        let output = invoke(arguments);
        assert!(!output.status.success());
        assert_eq!(output.stdout, b"");
        let program = env!("CARGO_BIN_EXE_wiiland-dump");
        let expected = format!(
            "Usage: {program} FILE\nRead a Wii Remote EEPROM file and write its contents to stdout.\n"
        );
        assert_eq!(output.stderr, expected.as_bytes());
    }
}

#[test]
fn full_record_has_newline_and_boundary_eof_has_none() {
    let path = temporary_file("full", &[0, 1, 2, 0x7f, 0x80, 0xfe, 0xff, 0x55]);
    let output = invoke(&[path.to_str().expect("UTF-8 temporary path")]);
    remove(path);

    assert!(output.status.success());
    assert_eq!(output.stderr, b"");
    assert_eq!(
        output.stdout,
        b"0x00000000: 0x00 0x01 0x02 0x7f 0x80 0xfe 0xff 0x55\n0x00000008: (eof)"
    );
}

#[test]
fn empty_file_is_success_with_eof_marker() {
    let path = temporary_file("empty", &[]);
    let output = invoke(&[path.to_str().expect("UTF-8 temporary path")]);
    remove(path);

    assert!(output.status.success());
    assert_eq!(output.stderr, b"");
    assert_eq!(output.stdout, b"0x00000000: (eof)");
}

#[test]
fn partial_record_reports_hex_offset_and_failure() {
    let path = temporary_file("partial", &[0x10, 0x20, 0xff]);
    let path_string = path.to_str().expect("UTF-8 temporary path").to_owned();
    let output = invoke(&[&path_string]);
    remove(path);

    assert!(!output.status.success());
    assert_eq!(output.stdout, b"0x00000000: 0x10 0x20 0xff (eof)");
    let expected = format!("Unexpected end of eeprom file '{path_string}' at offset 0x00000003\n");
    assert_eq!(output.stderr, expected.as_bytes());
}
