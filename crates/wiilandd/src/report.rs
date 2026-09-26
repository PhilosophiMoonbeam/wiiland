//! Finite host and hardware diagnostics used by `wiilandd-hardware-report`.
//!
//! This module deliberately uses `std::process::Command` rather than a shell.  Besides
//! avoiding shell injection, that keeps command arguments and exit statuses observable
//! for report consumers and tests.

use std::env;
use std::fmt::Write as _;
#[cfg(not(windows))]
use std::fs::{self, OpenOptions};
#[cfg(not(windows))]
use std::io::{self, Read, Write};
#[cfg(not(windows))]
use std::os::unix::fs::PermissionsExt;
#[cfg(not(windows))]
use std::os::unix::process::CommandExt;
#[cfg(not(windows))]
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(not(windows))]
use std::process::Stdio;
#[cfg(not(windows))]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(not(windows))]
const USAGE: &str = "Usage:\n  wiilandd-hardware-report\n  wiilandd-hardware-report <number-or-/sys/path> [extra wiilandd args]\n\nCollect finite WiiLand host, permission, config, doctor, axis-map, device, and\nmanual Wayland/X.org validation diagnostics. With a device argument, continue\ninto live dry-run trace capture and pass non-conflicting arguments to wiilandd.";

#[cfg(windows)]
const USAGE: &str = "Usage:\n  wiilandd-hardware-report\n  wiilandd-hardware-report <number-or-HID-identity> [extra wiilandd args]\n\nCollect Windows configuration, backend, HID device, and runtime dependency\ndiagnostics. With a device selector, request live dry-run trace capture from\nwiilandd using an ordinal or opaque Windows HID identity.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedArgs {
    pub device: Option<String>,
    pub extra: Vec<String>,
    pub trace_selectors: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgError {
    Conflicting(String),
    MultipleTraceSelectors,
}

impl std::fmt::Display for ArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflicting(arg) => write!(f, "conflicting trace argument: {arg}"),
            Self::MultipleTraceSelectors => write!(f, "exactly one trace selector is allowed"),
        }
    }
}

/// Parse the report's positional device and the arguments forwarded to the trace.
///
/// This intentionally mirrors the established scanner: only an initial `--help` is
/// help, value-taking options consume their next argument, and all other unknown
/// options are passed through to the daemon.
pub fn parse_args(args: &[String]) -> Result<ParsedArgs, ArgError> {
    let (device, rest) = match args.split_first() {
        None => (None, &[][..]),
        Some((first, _)) if first == "-h" || first == "--help" => {
            return Ok(ParsedArgs {
                device: None,
                extra: vec![first.clone()],
                trace_selectors: 0,
            });
        }
        Some((first, rest)) => (Some(first.clone()), rest),
    };

    let mut extra = Vec::with_capacity(rest.len());
    let mut selectors = 0;
    let mut i = 0;
    while i < rest.len() {
        let arg = &rest[i];
        if arg == "--trace-events" || arg.starts_with("--trace-events=") {
            selectors += 1;
        }
        if is_conflicting(arg) {
            return Err(ArgError::Conflicting(arg.clone()));
        }
        extra.push(arg.clone());
        if consumes_value(arg) && i + 1 < rest.len() {
            i += 1;
            extra.push(rest[i].clone());
        }
        i += 1;
    }
    if selectors > 1 {
        return Err(ArgError::MultipleTraceSelectors);
    }
    Ok(ParsedArgs {
        device,
        extra,
        trace_selectors: selectors,
    })
}

fn is_conflicting(arg: &str) -> bool {
    matches!(
        arg,
        "-d" | "--device"
            | "-p"
            | "--profile"
            | "-n"
            | "--dry-run"
            | "-h"
            | "--help"
            | "--version"
            | "-l"
            | "--list"
            | "--calibrate-aim"
            | "--check-config"
            | "--self-test"
            | "--axis-map"
            | "--validation-checklist"
            | "--doctor"
            | "--dump-config"
            | "--pair"
            | "--radio"
            | "--pairing-method"
    ) || arg.starts_with("--device=")
        || arg.starts_with("--profile=")
        || arg.starts_with("--dry-run=")
        || arg.starts_with("--radio=")
        || arg.starts_with("--pairing-method=")
}

fn consumes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-c" | "--config"
            | "--backend"
            | "--ir-speed"
            | "--ir-deadzone"
            | "--ir-smoothing"
            | "--ir-tracking"
            | "--ir-aim-mapping"
            | "--pointer-speed"
            | "--aim-mode"
            | "--aim-source"
            | "--aim-activation"
            | "--aim-sensitivity"
            | "--aim-deadzone"
            | "--aim-smoothing"
            | "--aim-invert-x"
            | "--aim-invert-y"
            | "--aim-calibration-duration"
            | "--radio"
            | "--pairing-method"
    )
}

#[cfg(not(windows))]
#[derive(Debug, Clone)]
pub struct ReportEnvironment {
    pub wiilandd: String,
    pub repo_dir: PathBuf,
    pub module_dir: PathBuf,
    pub os_release: PathBuf,
    pub tmp_dir: PathBuf,
}

#[cfg(not(windows))]
impl ReportEnvironment {
    pub fn from_env() -> Self {
        let repo_dir = env::var_os("WIILAND_REPO_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        Self {
            wiilandd: env::var("WIILANDD").unwrap_or_else(|_| "wiilandd".to_owned()),
            repo_dir,
            module_dir: env::var_os("HID_WIIMOTE_MODULE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/sys/module/hid_wiimote")),
            os_release: env::var_os("OS_RELEASE_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/etc/os-release")),
            tmp_dir: env::var_os("TMPDIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp")),
        }
    }
}

#[cfg(not(windows))]
struct TempDir(PathBuf);
#[cfg(not(windows))]
impl TempDir {
    fn create(parent: &Path) -> io::Result<Self> {
        fs::create_dir_all(parent)?;
        let pid = std::process::id();
        for n in 0..1000_u32 {
            let path = parent.join(format!("wiilandd-hardware-report.{pid}.{n:03}"));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "unable to create report temporary directory",
        ))
    }
    fn cleanup(self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[cfg(not(windows))]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(not(windows))]
fn command_available(program: &str) -> bool {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
    }
    let Some(paths) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&paths).any(|dir| {
        let candidate = dir.join(program);
        fs::metadata(candidate)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

#[cfg(not(windows))]
fn command(program: &str, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command.args(args);
    command
}

#[cfg(not(windows))]
fn display_command(out: &mut String, prefix: &str, program: &str, args: &[String]) {
    let _ = write!(out, "{prefix} {program}");
    for arg in args {
        let _ = write!(out, " {arg}");
    }
    out.push('\n');
}

#[cfg(not(windows))]
fn run_optional(out: &mut String, program: &str, args: &[&str]) {
    let mut shown = vec![program.to_owned()];
    shown.extend(args.iter().map(|a| (*a).to_owned()));
    let _ = writeln!(out, "$ optional: {}", shown.join(" "));
    if !command_available(program) {
        let _ = writeln!(out, "optional unavailable: {program}");
        return;
    }
    let result = command(program, args).stderr(Stdio::inherit()).output();
    let (ok, stdout) = match result {
        Ok(output) => (output.status.success(), output.stdout),
        Err(_) => (false, Vec::new()),
    };
    out.push_str(&String::from_utf8_lossy(&stdout));
    if !ok {
        let _ = writeln!(out, "optional failed: {}", shown.join(" "));
    }
}

#[cfg(not(windows))]
fn run_probe(out: &mut String, program: &str, args: &[String]) -> bool {
    display_command(out, "$", program, args);
    let mut c = Command::new(program);
    c.args(args).stderr(Stdio::inherit());
    let result = c.output();
    let (ok, stdout) = match result {
        Ok(output) => (output.status.success(), output.stdout),
        Err(_) => (false, Vec::new()),
    };
    out.push_str(&String::from_utf8_lossy(&stdout));
    if !ok {
        let _ = writeln!(out, "failed: {program} {}", args.join(" "));
    }
    ok
}

#[cfg(not(windows))]
fn run_required(
    out: &mut String,
    failures: &mut usize,
    program: &str,
    args: &[String],
    name: &str,
) {
    if !run_probe(out, program, args) {
        *failures += 1;
        let _ = writeln!(out, "core.failure.{failures}=wiilandd.{name}");
    }
}

#[cfg(not(windows))]
fn read_attr(path: &Path, attr: &str) -> String {
    let mut value = String::new();
    match fs::File::open(path.join(attr)).and_then(|mut f| f.read_to_string(&mut value)) {
        Ok(_) => value.trim_end_matches('\n').to_owned(),
        Err(_) => "unavailable".to_owned(),
    }
}

#[cfg(not(windows))]
fn battery(path: &Path) -> String {
    let Ok(entries) = fs::read_dir(path.join("power_supply")) else {
        return "unavailable".to_owned();
    };
    let mut dirs: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for dir in dirs {
        let attr = dir.join("capacity");
        if attr.is_file() {
            let mut value = String::new();
            if fs::File::open(attr)
                .and_then(|mut f| f.read_to_string(&mut value))
                .is_ok()
            {
                return value.trim_end_matches('\n').to_owned();
            }
        }
    }
    "unavailable".to_owned()
}

#[cfg(not(windows))]
fn path_access(out: &mut String, label: &str, path: &Path) {
    let exists = path.exists();
    let _ = writeln!(out, "{label}.exists={}", if exists { "yes" } else { "no" });
    let readable = fs::File::open(path).is_ok();
    let writable = OpenOptions::new().write(true).open(path).is_ok();
    let _ = writeln!(
        out,
        "{label}.readable={}",
        if readable { "yes" } else { "no" }
    );
    let _ = writeln!(
        out,
        "{label}.writable={}",
        if writable { "yes" } else { "no" }
    );
    if exists {
        if let Ok(metadata) = fs::metadata(path) {
            let _ = writeln!(
                out,
                "{label}.mode={:o}",
                metadata.permissions().mode() & 0o7777
            );
        }
        for (field, fmt) in [("owner", "%U"), ("group", "%G")] {
            let output = Command::new("stat").arg("-c").arg(fmt).arg(path).output();
            if let Ok(output) = output
                && output.status.success()
            {
                let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                let _ = writeln!(out, "{label}.{field}={value}");
            }
        }
    }
}

#[cfg(not(windows))]
fn report_event_nodes(out: &mut String, index: &str, syspath: &Path) {
    let Ok(inputs) = fs::read_dir(syspath.join("input")) else {
        return;
    };
    let mut input_dirs: Vec<_> = inputs
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("input"))
                && p.is_dir()
        })
        .collect();
    input_dirs.sort();
    for input in input_dirs {
        let Ok(events) = fs::read_dir(input) else {
            continue;
        };
        let mut event_dirs: Vec<_> = events
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("event"))
                    && p.is_dir()
            })
            .collect();
        event_dirs.sort();
        for event_dir in event_dirs {
            let Some(event) = event_dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let node = PathBuf::from("/dev/input").join(event);
            let _ = writeln!(out, "device.{index}.event.{event}.node={}", node.display());
            path_access(out, &format!("device.{index}.event.{event}"), &node);
        }
    }
}

#[cfg(not(windows))]
fn report_device_uevent(out: &mut String, index: &str, syspath: &Path) {
    let Ok(contents) = fs::read_to_string(syspath.join("uevent")) else {
        let _ = writeln!(out, "device.{index}.uevent=unavailable");
        return;
    };
    for line in contents.lines() {
        if [
            "HID_ID=",
            "HID_NAME=",
            "HID_PHYS=",
            "HID_UNIQ=",
            "MODALIAS=",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        {
            let _ = writeln!(out, "device.{index}.uevent.{line}");
        }
    }
}

#[cfg(not(windows))]
fn report_device_attrs(out: &mut String, failures: &mut usize, contents: &str) {
    for (line_number, line) in contents.lines().enumerate() {
        let row = line_number + 1;
        if line.trim().is_empty() || line == "No Wii Remote devices found" {
            continue;
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        let index = fields.first().copied().unwrap_or("");
        let syspath = fields.get(1).copied().unwrap_or("");
        let extra = if fields.len() > 2 {
            fields[2..].join(" ")
        } else {
            String::new()
        };
        let reason = if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
            Some("invalid-index")
        } else if syspath.is_empty() {
            Some("missing-syspath")
        } else if fields.len() > 2 {
            Some("extra-fields")
        } else {
            None
        };
        if let Some(reason) = reason {
            let _ = writeln!(out, "device-list.row.{row}.malformed={reason}");
            let _ = writeln!(
                out,
                "device-list.row.{row}.parsed={index}|{syspath}|{extra}"
            );
            *failures += 1;
            let _ = writeln!(
                out,
                "core.failure.{failures}=wiilandd.list.malformed-row.{row}"
            );
            continue;
        }
        let path = Path::new(syspath);
        let _ = writeln!(out, "device.{index}.syspath={syspath}");
        let _ = writeln!(out, "device.{index}.devtype={}", read_attr(path, "devtype"));
        let _ = writeln!(
            out,
            "device.{index}.extension={}",
            read_attr(path, "extension")
        );
        let _ = writeln!(out, "device.{index}.battery={}", battery(path));
        report_device_uevent(out, index, path);
        report_event_nodes(out, index, path);
    }
}

#[cfg(not(windows))]
fn report_os_release(out: &mut String, path: &Path) {
    let Ok(contents) = fs::read_to_string(path) else {
        let _ = writeln!(out, "os-release=unavailable");
        return;
    };
    let _ = writeln!(out, "os-release.path={}", path.display());
    for line in contents.lines() {
        if [
            "NAME=",
            "PRETTY_NAME=",
            "ID=",
            "VERSION_ID=",
            "VERSION_CODENAME=",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        {
            let _ = writeln!(out, "os-release.{line}");
        }
    }
}

#[cfg(not(windows))]
fn report_module_parameters(out: &mut String, module_dir: &Path) {
    let parameters = module_dir.join("parameters");
    let Ok(entries) = fs::read_dir(&parameters) else {
        let _ = writeln!(out, "hid-wiimote.parameters=unavailable");
        return;
    };
    let _ = writeln!(out, "hid-wiimote.module_dir={}", module_dir.display());
    let mut attrs: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    attrs.sort();
    for attr in attrs {
        if !attr.exists() {
            continue;
        }
        let Some(name) = attr.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let value = if attr.is_file() {
            read_attr(&parameters, name)
        } else {
            "unreadable".to_owned()
        };
        let _ = writeln!(out, "hid-wiimote.parameter.{name}={value}");
    }
}

#[cfg(not(windows))]
fn report_git(out: &mut String, repo: &Path) {
    if !command_available("git") {
        let _ = writeln!(out, "git.commit=unavailable\ngit.dirty=unavailable");
        return;
    }
    let commit = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--short", "HEAD"])
        .stderr(Stdio::null())
        .output();
    let _ = writeln!(
        out,
        "git.commit={}",
        commit
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unavailable".to_owned())
    );
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .stderr(Stdio::null())
        .output();
    let dirty = match status {
        Ok(o) if o.status.success() => {
            if o.stdout.is_empty() {
                "no"
            } else {
                "yes"
            }
        }
        _ => "unavailable",
    };
    let _ = writeln!(out, "git.dirty={dirty}");
}

#[cfg(not(windows))]
fn append_optional_output(out: &mut String, shown: &str, output: &std::process::Output) -> bool {
    out.push_str(&String::from_utf8_lossy(&output.stdout));
    if output.status.success() {
        true
    } else {
        let _ = writeln!(out, "optional failed: {shown}");
        false
    }
}

#[cfg(not(windows))]
fn bluetooth(out: &mut String) {
    if !command_available("bluetoothctl") {
        let _ = writeln!(out, "optional.bluetoothctl.controllers=unavailable");
        return;
    }
    let _ = writeln!(out, "$ optional: bluetoothctl list");
    let output = Command::new("bluetoothctl").arg("list").output();
    let Ok(output) = output else {
        let _ = writeln!(out, "optional failed: bluetoothctl list");
        return;
    };
    out.push_str(&String::from_utf8_lossy(&output.stdout));
    if !output.status.success() {
        let _ = writeln!(out, "optional failed: bluetoothctl list");
        return;
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("Controller") {
            continue;
        }
        let Some(address) = fields.next() else {
            continue;
        };
        let shown = format!("bluetoothctl show {address}");
        let _ = writeln!(out, "$ optional: {shown}");
        let output = Command::new("bluetoothctl")
            .args(["show", address])
            .output();
        match output {
            Ok(output) => {
                append_optional_output(out, &shown, &output);
            }
            Err(_) => {
                let _ = writeln!(out, "optional failed: {shown}");
            }
        }
    }
}

#[cfg(not(windows))]
fn timestamp() -> String {
    // UTC calendar conversion from Unix seconds, avoiding a locale-dependent subprocess.
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = seconds / 86_400;
    let rem = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}
#[cfg(not(windows))]
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

#[cfg(not(windows))]
fn manual(out: &mut String) {
    out.push_str("\n== manual-validation ==\n");
    out.push_str("manual.sdl=TODO: validate virtual gamepad in an SDL input tester\n");
    out.push_str("manual.wine-proton=TODO: validate virtual gamepad in one Wine/Proton game\n");
    out.push_str("manual.native-wayland-desktop=TODO: validate desktop profile pointer/buttons under a native Wayland compositor\n");
    out.push_str("manual.native-xorg-desktop=TODO: validate desktop profile pointer/buttons in a native X.org session\n");
    out.push_str("manual.native-x11-consumer=TODO: validate a native X11 application in a native X.org session\n");
    out.push_str("manual.xwayland-consumer=TODO: validate an X11 application through XWayland in a Wayland session\n");
    out.push_str(
        "manual.steam-motion-aim=TODO: validate aim-mode=right-stick in one Steam Input game\n",
    );
    out.push_str("manual.nonsteam-motion-aim=TODO: validate aim-mode=right-stick in one native or XWayland non-Steam game\n");
    out.push_str("manual.mouse-motion-aim=TODO: validate aim-mode=mouse in one game that accepts mouse aim\n");
    out.push_str("manual.motion-aim-calibration=TODO: run wiilandd --device <N> --calibrate-aim on a flat stable surface and paste generated offsets into the test config\n");
    out.push_str("manual.ir-screen-calibration=optional: for absolute IR aim, record ir-screen-left/right/top/bottom and sensor bar placement used during validation\n");
    out.push_str("manual.notes=TODO: record pass/fail details, game/app names, display server, and deviations\n");
}

pub fn usage() -> &'static str {
    USAGE
}

/// Run the finite report. The returned value is the process exit status. When a device
/// is selected, successful report collection ends by replacing this process with the
/// daemon, preserving its PID, signals, and final status.
#[cfg(not(windows))]
pub fn run() -> i32 {
    let raw: Vec<String> = env::args().skip(1).collect();
    if raw.first().is_some_and(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return 0;
    }
    let parsed = match parse_args(&raw) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("wiilandd-hardware-report: {error}");
            eprintln!("{USAGE}");
            return 2;
        }
    };
    let environment = ReportEnvironment::from_env();
    let temporary = match TempDir::create(&environment.tmp_dir) {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("wiilandd-hardware-report: {error}");
            return 1;
        }
    };
    let mut out = String::new();
    let mut failures = 0;
    out.push_str("\n== host ==\nreport.schema.version=2\n");
    let _ = writeln!(out, "report.timestamp.utc={}", timestamp());
    run_optional(&mut out, "uname", &["-srmo"]);
    report_os_release(&mut out, &environment.os_release);
    run_optional(&mut out, "bluetoothctl", &["--version"]);
    bluetooth(&mut out);
    run_optional(&mut out, "modinfo", &["hid-wiimote"]);
    report_module_parameters(&mut out, &environment.module_dir);
    if env::var_os("XDG_SESSION_ID").is_some() && command_available("loginctl") {
        let id = env::var("XDG_SESSION_ID").unwrap_or_default();
        run_optional(
            &mut out,
            "loginctl",
            &[
                "show-session",
                &id,
                "-p",
                "Type",
                "-p",
                "Desktop",
                "-p",
                "Name",
            ],
        );
    } else {
        out.push_str("optional.session-probe=unavailable\n");
    }
    for key in [
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "XDG_SESSION_TYPE",
        "DESKTOP_SESSION",
        "GDMSESSION",
        "KDE_SESSION_VERSION",
        "WAYLAND_DISPLAY",
        "DISPLAY",
    ] {
        let _ = writeln!(out, "{key}={}", env::var(key).unwrap_or_default());
    }
    if let Some(value) = env::var_os("XAUTHORITY") {
        let _ = writeln!(
            out,
            "XAUTHORITY.set=yes\nXAUTHORITY.readable={}",
            if !value.is_empty() && Path::new(&value).is_file() && fs::File::open(&value).is_ok() {
                "yes"
            } else {
                "no"
            }
        );
    } else {
        out.push_str("XAUTHORITY.set=no\nXAUTHORITY.readable=no\n");
    }
    for key in ["SWAYSOCK", "HYPRLAND_INSTANCE_SIGNATURE"] {
        let _ = writeln!(out, "{key}={}", env::var(key).unwrap_or_default());
    }
    out.push_str("\n== permissions ==\n");
    run_optional(&mut out, "id", &[]);
    path_access(&mut out, "dev.uinput", Path::new("/dev/uinput"));
    out.push_str("\n== wiilandd ==\n");
    if command_available(&environment.wiilandd) {
        out.push_str("wiilandd.available=yes\n");
    } else {
        out.push_str("wiilandd.available=no\n");
        failures += 1;
        let _ = writeln!(out, "core.failure.{failures}=wiilandd.missing");
    }
    let _ = run_probe(&mut out, &environment.wiilandd, &["--version".to_owned()]);
    report_git(&mut out, &environment.repo_dir);
    run_required(
        &mut out,
        &mut failures,
        &environment.wiilandd,
        &["--check-config".to_owned()],
        "--check-config",
    );
    run_required(
        &mut out,
        &mut failures,
        &environment.wiilandd,
        &["--dump-config".to_owned()],
        "--dump-config",
    );
    let _ = run_probe(&mut out, &environment.wiilandd, &["--axis-map".to_owned()]);
    run_required(
        &mut out,
        &mut failures,
        &environment.wiilandd,
        &["--validation-checklist".to_owned()],
        "--validation-checklist",
    );
    run_required(
        &mut out,
        &mut failures,
        &environment.wiilandd,
        &["--doctor".to_owned()],
        "--doctor",
    );
    out.push_str("\n== devices ==\n");
    display_command(&mut out, "$", &environment.wiilandd, &["--list".to_owned()]);
    let list = Command::new(&environment.wiilandd).arg("--list").output();
    match list {
        Ok(output) if output.status.success() => {
            let contents = String::from_utf8_lossy(&output.stdout);
            out.push_str(&contents);
            report_device_attrs(&mut out, &mut failures, &contents);
        }
        _ => {
            out.push_str(&format!("failed: {} --list\n", environment.wiilandd));
            failures += 1;
            let _ = writeln!(out, "core.failure.{failures}=wiilandd.list");
        }
    }
    manual(&mut out);
    out.push_str("\n== result ==\n");
    let _ = writeln!(out, "report.core-failures={failures}");
    if failures > 0 {
        out.push_str("report.status=failed\n");
        print!("{out}");
        temporary.cleanup();
        return 1;
    }
    out.push_str("report.status=ok\n");
    if parsed.device.is_none() {
        out.push_str("\nPass a device number or sysfs path to capture live dry-run traces:\n");
        let _ = writeln!(
            out,
            "  WIILANDD={} wiilandd-hardware-report <number-or-/sys/path> [extra wiilandd args]",
            environment.wiilandd
        );
        let _ = writeln!(
            out,
            "For focused traces:\n  WIILANDD={} wiilandd-hardware-report <number-or-/sys/path> --trace-events=motion-plus\n  WIILANDD={} wiilandd-hardware-report <number-or-/sys/path> --trace-events=ir",
            environment.wiilandd, environment.wiilandd
        );
        out.push_str("\nDuring trace capture, exercise every button, stick, trigger, accelerometer,\nMotionPlus axis, IR pointer source, and attached extension. Stop with Ctrl-C.\n");
        print!("{out}");
        temporary.cleanup();
        return 0;
    }
    let device = parsed.device.unwrap_or_default();
    let _ = writeln!(
        out,
        "\n== trace ==\nTracing {device}. Stop with Ctrl-C after exercising the hardware matrix."
    );
    print!("{out}");
    let _ = io::stdout().flush();
    temporary.cleanup();
    let mut args = vec!["--dry-run".to_owned()];
    if parsed.trace_selectors == 0 {
        args.push("--trace-events".to_owned());
    }
    args.extend([
        "--verbose".to_owned(),
        "--device".to_owned(),
        device,
        "--profile".to_owned(),
        "both".to_owned(),
    ]);
    args.extend(parsed.extra);
    let mut command = Command::new(&environment.wiilandd);
    command.args(&args);
    let error = command.exec();
    eprintln!(
        "wiilandd-hardware-report: failed to exec {}: {error}",
        environment.wiilandd
    );
    1
}

#[cfg(windows)]
fn run_windows_command(
    out: &mut String,
    executable: &str,
    args: &[&str],
    required: bool,
    failures: &mut usize,
    name: &str,
) {
    let _ = write!(out, "$ {executable}");
    for arg in args {
        let _ = write!(out, " {arg}");
    }
    out.push('\n');
    match Command::new(executable).args(args).output() {
        Ok(output) => {
            out.push_str(&String::from_utf8_lossy(&output.stdout));
            out.push_str(&String::from_utf8_lossy(&output.stderr));
            if !output.status.success() {
                let _ = writeln!(
                    out,
                    "failed: {executable} {} (status {})",
                    args.join(" "),
                    output.status
                );
                if required {
                    *failures += 1;
                    let _ = writeln!(out, "core.failure.{failures}=wiilandd.{name}");
                }
            }
        }
        Err(error) => {
            let _ = writeln!(out, "failed: {executable}: {error}");
            if required {
                *failures += 1;
                let _ = writeln!(out, "core.failure.{failures}=wiilandd.{name}");
            }
        }
    }
}

#[cfg(windows)]
pub fn run() -> i32 {
    let raw: Vec<String> = env::args().skip(1).collect();
    if raw
        .first()
        .is_some_and(|arg| arg == "-h" || arg == "--help")
    {
        println!("{USAGE}");
        return 0;
    }
    let parsed = match parse_args(&raw) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("wiilandd-hardware-report: {error}");
            eprintln!("{USAGE}");
            return 2;
        }
    };
    let executable = env::var("WIILANDD").unwrap_or_else(|_| "wiilandd.exe".to_owned());
    let mut out = String::from(
        "\n== Windows runtime ==\nreport.schema.version=2\nruntime.output.service=wiiland-output-service\nruntime.output.driver=wiiland-vhid\nruntime.output.installation=not-probed\nruntime.output.probe=not-performed-by-report\n",
    );
    let mut failures = 0;
    run_windows_command(
        &mut out,
        &executable,
        &["--version"],
        false,
        &mut failures,
        "version",
    );
    run_windows_command(
        &mut out,
        &executable,
        &["--check-config"],
        true,
        &mut failures,
        "check-config",
    );
    run_windows_command(
        &mut out,
        &executable,
        &["--dump-config"],
        false,
        &mut failures,
        "dump-config",
    );
    run_windows_command(
        &mut out,
        &executable,
        &["--axis-map"],
        false,
        &mut failures,
        "axis-map",
    );
    run_windows_command(
        &mut out,
        &executable,
        &["--validation-checklist"],
        false,
        &mut failures,
        "validation-checklist",
    );
    run_windows_command(
        &mut out,
        &executable,
        &["--doctor"],
        true,
        &mut failures,
        "doctor",
    );
    run_windows_command(
        &mut out,
        &executable,
        &["--list"],
        true,
        &mut failures,
        "list",
    );

    if let Some(device) = parsed.device.as_deref() {
        let _ = writeln!(
            out,
            "\n== trace ==\nTracing Windows HID selector {device}; stop with Ctrl-C."
        );
        let mut args = vec!["--dry-run".to_owned()];
        if parsed.trace_selectors == 0 {
            args.push("--trace-events".to_owned());
        }
        args.extend([
            "--verbose".to_owned(),
            "--device".to_owned(),
            device.to_owned(),
            "--profile".to_owned(),
            "both".to_owned(),
        ]);
        args.extend(parsed.extra);
        let borrowed = args.iter().map(String::as_str).collect::<Vec<_>>();
        run_windows_command(
            &mut out,
            &executable,
            &borrowed,
            true,
            &mut failures,
            "trace",
        );
    } else {
        out.push_str(
            "\nPass a device ordinal or opaque HID identity to capture live dry-run traces:\n",
        );
        let _ = writeln!(
            out,
            "  WIILANDD={executable} wiilandd-hardware-report <number-or-HID-identity> [extra wiilandd args]"
        );
    }
    let _ = writeln!(out, "\nreport.core-failures={failures}");
    if failures == 0 {
        out.push_str("report.status=ok\n");
        print!("{out}");
        0
    } else {
        out.push_str("report.status=failed\n");
        print!("{out}");
        1
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_preserves_trace_order_and_consumes_values() {
        let args = vec![
            "7".into(),
            "--config".into(),
            "x".into(),
            "--trace-events=ir".into(),
        ];
        let parsed = parse_args(&args).unwrap();
        assert_eq!(parsed.device.as_deref(), Some("7"));
        assert_eq!(parsed.extra, vec!["--config", "x", "--trace-events=ir"]);
        assert_eq!(parsed.trace_selectors, 1);
    }
    #[test]
    fn scanner_rejects_conflicts_and_multiple_selectors() {
        for alias in [
            "-p",
            "--profile",
            "-n",
            "--dry-run",
            "--pair",
            "--radio",
            "--pairing-method",
        ] {
            let args = vec!["7".into(), alias.into()];
            assert_eq!(
                parse_args(&args),
                Err(ArgError::Conflicting(alias.into())),
                "{alias}"
            );
        }
        let args = vec!["7".into(), "--device".into()];
        assert_eq!(
            parse_args(&args),
            Err(ArgError::Conflicting("--device".into()))
        );
        let args = vec![
            "7".into(),
            "--trace-events=ir".into(),
            "--trace-events=axes".into(),
        ];
        assert_eq!(parse_args(&args), Err(ArgError::MultipleTraceSelectors));
    }

    #[cfg(not(windows))]
    #[test]
    fn captured_optional_output_stays_in_schema_order() {
        use std::os::unix::process::ExitStatusExt;

        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"Controller details\n".to_vec(),
            stderr: Vec::new(),
        };
        let mut report = "$ optional: bluetoothctl show AA:BB\n".to_owned();

        assert!(append_optional_output(
            &mut report,
            "bluetoothctl show AA:BB",
            &output
        ));
        report.push_str("optional.next=value\n");

        assert_eq!(
            report,
            "$ optional: bluetoothctl show AA:BB\nController details\noptional.next=value\n"
        );
    }
}
