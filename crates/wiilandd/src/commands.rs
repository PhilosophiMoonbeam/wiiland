use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cli::{Action, Cli, IpcMode};
use wiiland_core::calibration::CalibrationStats;
use wiiland_core::mapping;
use wiiland_core::{Config, Profile, TraceEvent, TraceFilter, TracePayload};
use wiiland_hid::{EventKind, Interface, InterfaceMask, Monitor, MonitorMode};

#[derive(Debug)]
pub struct CommandError {
    code: i32,
    message: String,
}
impl CommandError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub fn code(&self) -> i32 {
        self.code
    }
}
impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for CommandError {}

fn io_errno(error: &io::Error) -> i32 {
    -error.raw_os_error().unwrap_or(libc::EIO)
}
pub fn execute(cli: &Cli) -> Result<(), CommandError> {
    match cli.action {
        Action::Help => {
            print!("{}", crate::cli::usage());
            Ok(())
        }
        Action::Version => {
            println!("wiilandd {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Action::List => list_devices(cli.verbose),
        Action::AxisMap => {
            print_axis_map();
            Ok(())
        }
        Action::ValidationChecklist => {
            print_validation_checklist();
            Ok(())
        }
        Action::Doctor => {
            print_doctor(&cli.config);
            Ok(())
        }
        Action::DumpConfig => {
            print!("{}", cli.config.dump());
            Ok(())
        }
        Action::CheckConfig => Ok(()),
        Action::SelfTest => self_test(),
        Action::CalibrateAim => calibrate_aim(cli),
        Action::Run => run_runtime(cli),
    }
}

fn run_runtime(cli: &Cli) -> Result<(), CommandError> {
    let device = resolve_run_device_with(cli.device.as_deref(), resolve_device_arg)?;
    let mut runtime = crate::runtime::Runtime::new(cli.config.clone()).map_err(runtime_error)?;
    runtime.set_dry_run(cli.dry_run);
    runtime.set_verbose(cli.verbose);
    runtime.set_trace(cli.trace);
    let default_path = if matches!(&cli.ipc, IpcMode::Auto) {
        Some(wiiland_ipc::default_socket_path())
    } else {
        None
    };
    if let Some(path) = ipc_path_for_mode(&cli.ipc, default_path)? {
        runtime.enable_ipc(path.as_path()).map_err(|error| {
            CommandError::new(
                error.raw_os_error().unwrap_or(libc::EIO),
                format!(
                    "wiilandd: cannot bind IPC socket {}: {error}",
                    path.display()
                ),
            )
        })?;
    }
    if let Some(device) = device {
        runtime.run_single(device).map_err(runtime_error)
    } else {
        runtime.run_monitor().map_err(runtime_error)
    }
}

fn ipc_path_for_mode(
    mode: &IpcMode,
    default_path: Option<Result<PathBuf, wiiland_ipc::ClientError>>,
) -> Result<Option<PathBuf>, CommandError> {
    match mode {
        IpcMode::Disabled => Ok(None),
        IpcMode::Path(path) => Ok(Some(path.clone())),
        IpcMode::Auto => {
            let Some(default_path) = default_path else {
                return Err(CommandError::new(
                    libc::EINVAL,
                    "wiilandd: default IPC socket path was not provided",
                ));
            };
            match default_path {
                Ok(path) => Ok(Some(path)),
                Err(wiiland_ipc::ClientError::RuntimeDirectoryMissing)
                | Err(wiiland_ipc::ClientError::RuntimeDirectoryRelative(_)) => Ok(None),
                Err(error) => Err(CommandError::new(
                    libc::EINVAL,
                    format!("wiilandd: cannot determine default IPC socket: {error}"),
                )),
            }
        }
    }
}

fn runtime_error(code: i32) -> CommandError {
    CommandError::new(
        code.unsigned_abs() as i32,
        format!("wiilandd: runtime failed: {}", code),
    )
}

fn resolve_run_device_with(
    arg: Option<&str>,
    mut resolve: impl FnMut(&str) -> Option<PathBuf>,
) -> Result<Option<PathBuf>, CommandError> {
    arg.map(|arg| {
        resolve(arg).ok_or_else(|| {
            CommandError::new(
                libc::ENODEV,
                "wiilandd: cannot resolve device; run --list and pass --device <number|/sys/path>",
            )
        })
    })
    .transpose()
}
fn list_devices(verbose: bool) -> Result<(), CommandError> {
    let mut monitor = Monitor::new(MonitorMode::Enumerate).map_err(|error| {
        CommandError::new(
            io_errno(&error).unsigned_abs() as i32,
            format!("wiilandd: cannot create monitor: {error}"),
        )
    })?;
    let mut count = 0u32;
    while let Some(path) = monitor.poll().map_err(|error| {
        CommandError::new(
            io_errno(&error).unsigned_abs() as i32,
            format!("wiilandd: cannot poll monitor: {error}"),
        )
    })? {
        count += 1;
        println!("{}\t{}", count, path.display());
        if verbose {
            print_list_attr(&path, "devtype");
            print_list_attr(&path, "extension");
        }
    }
    if count == 0 {
        println!("No Wii Remote devices found");
    }
    Ok(())
}
fn print_list_attr(path: &Path, name: &str) {
    let value = fs::read_to_string(path.join(name))
        .ok()
        .map(|x| x.trim().to_owned());
    println!(
        "\t{}={}",
        name,
        value
            .as_deref()
            .filter(|x| !x.is_empty())
            .unwrap_or("unavailable")
    );
}

pub fn resolve_device_arg(arg: &str) -> Option<PathBuf> {
    let mut monitor = None;
    resolve_device_arg_with(arg, || {
        if monitor.is_none() {
            monitor = Monitor::new(MonitorMode::Enumerate).ok();
        }
        monitor
            .as_mut()
            .and_then(|monitor| monitor.poll().ok().flatten())
    })
}

fn resolve_device_arg_with(
    arg: &str,
    mut next_device: impl FnMut() -> Option<PathBuf>,
) -> Option<PathBuf> {
    if arg.starts_with('/') {
        return Some(PathBuf::from(arg));
    }
    let number = arg.parse::<usize>().ok().filter(|n| *n != 0)?;
    for index in 1..=number {
        let path = next_device()?;
        if index == number {
            return Some(path);
        }
    }
    None
}

fn calibrate_aim(cli: &Cli) -> Result<(), CommandError> {
    let path = cli.device.as_deref().and_then(resolve_device_arg).or_else(|| resolve_device_arg("1"))
        .ok_or_else(|| CommandError::new(libc::ENODEV, "wiilandd: cannot resolve calibration device; run --list and pass --device <number|/sys/path>"))?;
    let mut iface = Interface::new(&path).map_err(|e| {
        CommandError::new(
            io_errno(&e).unsigned_abs() as i32,
            format!("wiilandd: cannot open {}: {}", path.display(), e),
        )
    })?;
    iface.watch(true).map_err(|error| {
        CommandError::new(
            io_errno(&error).unsigned_abs() as i32,
            format!("wiilandd: cannot watch calibration device: {error}"),
        )
    })?;
    let available = iface.available() & (InterfaceMask::ACCEL | InterfaceMask::MOTION_PLUS);
    if available.is_empty() {
        return Err(CommandError::new(
            libc::ENODEV,
            "wiilandd: calibration device has no accelerometer or MotionPlus",
        ));
    }
    let opened_result = iface.open(available);
    let opened = match &opened_result {
        Ok(mask) => *mask,
        Err(error) => error.opened(),
    } & available;
    if opened.is_empty() {
        return Err(CommandError::new(
            libc::ENODEV,
            "wiilandd: cannot open calibration interfaces",
        ));
    }
    if let Err(e) = opened_result {
        eprintln!(
            "wiilandd: warning: some calibration interfaces unavailable: {}",
            e
        );
    }
    eprintln!(
        "wiilandd: place the Wii Remote face down, buttons against a flat stable surface, and keep it still for {} seconds",
        cli.config.aim_calibration_duration
    );
    let deadline = Instant::now() + Duration::from_secs(cli.config.aim_calibration_duration as u64);
    let mut accel = CalibrationStats::new();
    let mut motion = CalibrationStats::new();
    while Instant::now() < deadline {
        match iface.dispatch() {
            Ok(event) => collect_calibration_event(event.kind, &mut accel, &mut motion)?,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => {
                return Err(CommandError::new(
                    io_errno(&e).unsigned_abs() as i32,
                    format!(
                        "wiilandd: calibration dispatch failed for {}: {}",
                        path.display(),
                        e
                    ),
                ));
            }
        }
    }
    let accel_cal = accel.finish();
    let motion_cal = motion.finish();
    if accel_cal.is_none() && motion_cal.is_none() {
        return Err(CommandError::new(
            libc::EAGAIN,
            "wiilandd: calibration failed: keep the Wii Remote flat and still; no stable accelerometer or MotionPlus window was captured",
        ));
    }
    println!("# WiiLand motion aim calibration\n# Place these key=value lines in wiilandd.conf.");
    println!(
        "# samples.accelerometer={} jitter.accelerometer={}",
        accel.samples,
        accel.jitter()
    );
    println!(
        "# samples.motion-plus={} jitter.motion-plus={}",
        motion.samples,
        motion.jitter()
    );
    if let Some(c) = accel_cal {
        print_calibration("aim-accel-zero", c);
    } else {
        println!("# warning: accelerometer calibration unavailable or unstable");
    }
    if let Some(c) = motion_cal {
        print_calibration("aim-motion-plus-bias", c);
    } else {
        println!("# warning: MotionPlus calibration unavailable or unstable");
    }
    Ok(())
}
fn collect_calibration_event(
    event: EventKind,
    accel: &mut CalibrationStats,
    motion: &mut CalibrationStats,
) -> Result<(), CommandError> {
    match event {
        EventKind::Accel(value) => accel.add([value.x, value.y, value.z]),
        EventKind::MotionPlus(value) => motion.add([value.x, value.y, value.z]),
        EventKind::Watch | EventKind::Gone => {
            return Err(CommandError::new(
                libc::ENODEV,
                "wiilandd: calibration interrupted by a device or interface change; reconnect and retry",
            ));
        }
        _ => {}
    }
    Ok(())
}
fn print_calibration(prefix: &str, c: wiiland_core::SensorCalibration) {
    println!(
        "{}-x={}\n{}-y={}\n{}-z={}",
        prefix, c.x, prefix, c.y, prefix, c.z
    );
}

fn path_state(path: Option<&Path>, mode: u8) -> &'static str {
    let Some(path) = path else { return "unknown" };
    match fs::metadata(path) {
        Ok(m) => {
            if mode == 0 {
                "yes"
            } else if m.file_type().is_socket() {
                "socket"
            } else {
                "other"
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if mode == 0 {
                "no"
            } else {
                "missing"
            }
        }
        Err(_) => "unknown",
    }
}
fn access_state(path: Option<&Path>, read: bool) -> &'static str {
    let Some(path) = path else { return "unknown" };
    let flag = if read { libc::R_OK } else { libc::W_OK };
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok();
    c.map(|c| {
        if unsafe { libc::access(c.as_ptr(), flag) } == 0 {
            "yes"
        } else {
            "no"
        }
    })
    .unwrap_or("unknown")
}
fn wayland_path(display: Option<&str>, runtime: Option<&str>) -> Option<PathBuf> {
    let d = display?;
    if d.starts_with('/') {
        Some(PathBuf::from(d))
    } else {
        Some(Path::new(runtime?).join(d))
    }
}
fn x11_path(display: Option<&str>) -> Option<PathBuf> {
    let d = display?;
    let colon = d.rfind(':')?;
    let host = &d[..colon];
    let number = &d[colon + 1..];
    let n = number.split('.').next()?;
    if n.is_empty() || !n.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let local = host.is_empty()
        || host == "unix"
        || host == "unix/"
        || host == "localhost"
        || host == "localhost.localdomain"
        || std::env::var("HOSTNAME").is_ok_and(|h| h == host);
    local.then(|| PathBuf::from(format!("/tmp/.X11-unix/X{}", n)))
}
fn print_doctor(config: &Config) {
    let wayland = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|x| !x.is_empty());
    let x11 = std::env::var("DISPLAY").ok().filter(|x| !x.is_empty());
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok();
    let session = std::env::var("XDG_SESSION_TYPE")
        .ok()
        .filter(|x| !x.is_empty());
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .filter(|x| !x.is_empty());
    let server = match session.as_deref() {
        Some("wayland") => "wayland",
        Some("x11") => "x11",
        _ if wayland.is_some() => "wayland",
        _ if x11.is_some() => "x11",
        _ => "headless",
    };
    let wp = wayland_path(wayland.as_deref(), runtime.as_deref());
    let xp = x11_path(x11.as_deref());
    let xwayland = if server != "wayland" {
        "not-applicable"
    } else if x11.is_none() {
        "no"
    } else {
        match xp.as_deref() {
            Some(p) if path_state(Some(p), 1) == "socket" => "yes",
            Some(_) => "no",
            None => "unknown",
        }
    };
    println!(
        "session.display-server={}\nsession.wayland={}\nsession.x11={}\nsession.xwayland.available={}\nsession.type={}\nsession.desktop={}",
        server,
        if wayland.is_some() { "yes" } else { "no" },
        if x11.is_some() { "yes" } else { "no" },
        xwayland,
        session.as_deref().unwrap_or("unknown"),
        desktop.as_deref().unwrap_or("unknown")
    );
    println!(
        "wayland.display={}\nxdg.runtime.dir={}\nwayland.socket.path={}\nwayland.socket.type={}\nwayland.socket.exists={}\nwayland.socket.readable={}\nwayland.socket.writable={}",
        wayland.as_deref().unwrap_or("unknown"),
        runtime.as_deref().unwrap_or("unknown"),
        wp.as_deref()
            .map_or("unknown".into(), |p| p.display().to_string()),
        path_state(wp.as_deref(), 1),
        path_state(wp.as_deref(), 0),
        access_state(wp.as_deref(), true),
        access_state(wp.as_deref(), false)
    );
    println!(
        "x11.display={}\nx11.socket.path={}\nx11.socket.type={}\nx11.socket.exists={}\nx11.socket.readable={}\nx11.socket.writable={}",
        x11.as_deref().unwrap_or("unknown"),
        xp.as_deref()
            .map_or("unknown".into(), |p| p.display().to_string()),
        path_state(xp.as_deref(), 1),
        path_state(xp.as_deref(), 0),
        access_state(xp.as_deref(), true),
        access_state(xp.as_deref(), false)
    );
    let system = Path::new(wiiland_core::config::SYSTEM_CONFIG_PATH);
    let user = user_config_path();
    println!(
        "config.system.path={}\nconfig.system.exists={}\nconfig.system.readable={}\nconfig.user.path={}\nconfig.user.exists={}\nconfig.user.readable={}",
        system.display(),
        path_state(Some(system), 0),
        access_state(Some(system), true),
        user.as_deref()
            .map_or("unknown".into(), |p| p.display().to_string()),
        path_state(user.as_deref(), 0),
        access_state(user.as_deref(), true)
    );
    let uinput = Path::new("/dev/uinput");
    println!(
        "dev.uinput.exists={}\ndev.uinput.readable={}\ndev.uinput.writable={}\nbackend={}\nprofile={}\naim.mode={}\naim.source={}\naim.activation={}",
        path_state(Some(uinput), 0),
        access_state(Some(uinput), true),
        access_state(Some(uinput), false),
        config.backend.as_str(),
        config.profile.as_str().unwrap_or("unknown"),
        config.aim_mode.as_str(),
        config.aim_source.as_str(),
        config.aim_activation.as_str()
    );
}
fn user_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|x| Path::new(x).is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|x| Path::new(x).is_absolute())
                .map(|x| {
                    let mut p = PathBuf::from(x);
                    p.push(".config");
                    p.into_os_string()
                })
        })?;
    Some(PathBuf::from(base).join("wiiland/wiilandd.conf"))
}

const AXIS_MAP: &str = "range.signed=-32768:32767\nrange.trigger=0:1023\nrange.balance=0:65535\nwiimote.dpad.left=BTN_DPAD_LEFT\nwiimote.dpad.right=BTN_DPAD_RIGHT\nwiimote.dpad.up=BTN_DPAD_UP\nwiimote.dpad.down=BTN_DPAD_DOWN\nwiimote.a=BTN_SOUTH\nwiimote.b=BTN_EAST\nwiimote.plus=BTN_START\nwiimote.minus=BTN_SELECT\nwiimote.home=BTN_MODE\nwiimote.one=BTN_1\nwiimote.two=BTN_2\nwiimote.accel.x=ABS_THROTTLE\nwiimote.accel.y=ABS_RUDDER\nwiimote.accel.z=ABS_WHEEL\nnunchuk.stick.x=ABS_X\nnunchuk.stick.y=ABS_Y\nnunchuk.accel.x=ABS_HAT1X\nnunchuk.accel.y=ABS_HAT1Y\nnunchuk.accel.z=ABS_HAT2X\nnunchuk.c=BTN_C\nnunchuk.z=BTN_Z\nmotion-plus.x=ABS_GAS\nmotion-plus.y=ABS_BRAKE\nmotion-plus.z=ABS_HAT0X\nclassic.left-stick.x=ABS_X\nclassic.left-stick.y=ABS_Y\naim.right-stick.x=ABS_RX\naim.right-stick.y=ABS_RY\naim.mouse.x=REL_X\naim.mouse.y=REL_Y\nclassic.right-stick.x=ABS_RX\nclassic.right-stick.y=ABS_RY\nclassic.trigger.left=ABS_Z\nclassic.trigger.right=ABS_RZ\nclassic.dpad.left=BTN_DPAD_LEFT\nclassic.dpad.right=BTN_DPAD_RIGHT\nclassic.dpad.up=BTN_DPAD_UP\nclassic.dpad.down=BTN_DPAD_DOWN\nclassic.a=BTN_SOUTH\nclassic.b=BTN_EAST\nclassic.x=BTN_NORTH\nclassic.y=BTN_WEST\nclassic.plus=BTN_START\nclassic.minus=BTN_SELECT\nclassic.home=BTN_MODE\nclassic.tl=BTN_TL\nclassic.tr=BTN_TR\nclassic.zl=BTN_TL2\nclassic.zr=BTN_TR2\npro.left-stick.x=ABS_X\npro.left-stick.y=ABS_Y\npro.right-stick.x=ABS_RX\npro.right-stick.y=ABS_RY\npro.zl=BTN_TL2\npro.zr=BTN_TR2\npro.dpad.left=BTN_DPAD_LEFT\npro.dpad.right=BTN_DPAD_RIGHT\npro.dpad.up=BTN_DPAD_UP\npro.dpad.down=BTN_DPAD_DOWN\npro.a=BTN_SOUTH\npro.b=BTN_EAST\npro.x=BTN_NORTH\npro.y=BTN_WEST\npro.plus=BTN_START\npro.minus=BTN_SELECT\npro.home=BTN_MODE\npro.tl=BTN_TL\npro.tr=BTN_TR\npro.thumbl=BTN_THUMBL\npro.thumbr=BTN_THUMBR\nguitar.stick.x=ABS_X\nguitar.stick.y=ABS_Y\nguitar.whammy=ABS_HAT3X\nguitar.fret-board=ABS_HAT3Y\nguitar.strum.up=BTN_STRUM_BAR_UP\nguitar.strum.down=BTN_STRUM_BAR_DOWN\nguitar.plus=BTN_START\nguitar.minus=BTN_SELECT\nguitar.fret.far-up=BTN_FRET_FAR_UP\nguitar.fret.up=BTN_FRET_UP\nguitar.fret.mid=BTN_FRET_MID\nguitar.fret.low=BTN_FRET_LOW\nguitar.fret.far-low=BTN_FRET_FAR_LOW\ndrums.pad.x=ABS_X\ndrums.pad.y=ABS_Y\ndrums.cymbal.left=ABS_RX\ndrums.cymbal.right=ABS_RY\ndrums.tom.left=ABS_Z\ndrums.tom.right=ABS_RZ\ndrums.tom.far-right=ABS_HAT3X\ndrums.bass=ABS_HAT3Y\ndrums.hi-hat=ABS_MISC\ndrums.plus=BTN_START\ndrums.minus=BTN_SELECT\nbalance.top-right=ABS_PRESSURE\nbalance.bottom-right=ABS_DISTANCE\nbalance.top-left=ABS_TILT_X\nbalance.bottom-left=ABS_TILT_Y\n";
const VALIDATION: &str = "original.core-buttons=required\noriginal.accelerometer=required\noriginal.ir-desktop-pointer=required\nmotion-plus-external.hotplug=required\nmotion-plus-external.axes=required\nmotion-plus-builtin.axes=required\nnunchuk.stick=required\nnunchuk.buttons=required\nnunchuk.accelerometer=required\nclassic.sticks=required\nclassic.triggers=required\nclassic.buttons=required\npro.sticks=required\npro.triggers=required\npro.buttons=required\nguitar.frets=required\nguitar.strum=required\nguitar.whammy=required\nguitar.stick=required\ndrums.pads=required\ndrums.cymbals-toms=required\ndrums.pedals=required\nbalance-board.sensors=required\nwayland.sdl=required\nwayland.wine-proton=required\nwayland.desktop-profile=required\nsteam.motion-aim-right-stick=required\nsteam.motion-aim-mouse=required\nnonsteam.motion-aim-right-stick=required\nnonsteam.motion-aim-mouse=required\n";
fn print_axis_map() {
    print!("{}", AXIS_MAP);
}
fn print_validation_checklist() {
    print!("{}", VALIDATION);
}

fn self_test() -> Result<(), CommandError> {
    if mapping::scale_signed_axis(-500, 500, 500) != mapping::VIRTUAL_AXIS_MIN
        || mapping::scale_signed_axis(500, 500, 500) != mapping::VIRTUAL_AXIS_MAX
        || mapping::scale_unsigned_axis(1023, 1023, 1023) != 1023
    {
        return Err(CommandError::new(
            libc::EINVAL,
            "wiilandd self-test: axis scaling failed",
        ));
    }
    if mapping::map_key(4) != Some(mapping::BTN_SOUTH) || mapping::map_key(99).is_some() {
        return Err(CommandError::new(
            libc::EINVAL,
            "wiilandd self-test: key mapping failed",
        ));
    }
    let mut config = Config::default();
    config
        .apply_line("self-test", 1, "profile=desktop")
        .map_err(|e| CommandError::new(libc::EINVAL, e.to_string()))?;
    config
        .apply_line("self-test", 2, "device.wiimote.profile=both")
        .map_err(|e| CommandError::new(libc::EINVAL, e.to_string()))?;
    if config.profile != Profile::DESKTOP
        || config.profile_for_syspath("/sys/wiimote") != Profile::BOTH
    {
        return Err(CommandError::new(
            libc::EINVAL,
            "wiilandd self-test: config rules failed",
        ));
    }
    let mut stats = CalibrationStats::new();
    for _ in 0..16 {
        stats.add([10, 20, 30]);
    }
    let c = stats
        .finish()
        .ok_or_else(|| CommandError::new(libc::EINVAL, "wiilandd self-test: calibration failed"))?;
    if (c.x, c.y, c.z) != (10, 20, 30) {
        return Err(CommandError::new(
            libc::EINVAL,
            "wiilandd self-test: calibration means failed",
        ));
    }
    let trace = TraceEvent::new(
        1,
        Some(1_000_001),
        "/sys/test",
        0,
        TracePayload::Key(wiiland_core::KeyPayload { code: 4, state: 1 }),
    );
    if !TraceFilter::Keys.matches(trace.event_type)
        || TraceFilter::Ir.matches(trace.event_type)
        || !trace.format_line().contains("seq=1")
    {
        return Err(CommandError::new(
            libc::EINVAL,
            "wiilandd self-test: trace contract failed",
        ));
    }
    println!("wiilandd self-test: ok");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibration_rejects_lifecycle_changes_even_after_stable_samples() {
        for interruption in [EventKind::Watch, EventKind::Gone] {
            let mut accel = CalibrationStats::new();
            let mut motion = CalibrationStats::new();
            for _ in 0..16 {
                collect_calibration_event(
                    EventKind::Accel(wiiland_hid::Axis3 { x: 1, y: 2, z: 3 }),
                    &mut accel,
                    &mut motion,
                )
                .unwrap();
            }
            assert!(accel.finish().is_some());
            let error =
                collect_calibration_event(interruption, &mut accel, &mut motion).unwrap_err();
            assert_eq!(error.code(), libc::ENODEV);
        }
    }

    #[test]
    fn normal_run_resolves_numeric_device_before_runtime() {
        let mut selector = None;
        let resolved = resolve_run_device_with(Some("2"), |arg| {
            selector = Some(arg.to_owned());
            Some(PathBuf::from("/sys/devices/second"))
        })
        .unwrap();

        assert_eq!(selector.as_deref(), Some("2"));
        assert_eq!(resolved.as_deref(), Some(Path::new("/sys/devices/second")));

        let error = resolve_run_device_with(Some("9"), |_| None).unwrap_err();
        assert_eq!(error.code(), libc::ENODEV);
        assert!(error.to_string().contains("cannot resolve device"));
    }

    #[test]
    fn numeric_device_resolution_uses_one_based_enumeration() {
        let mut devices = [
            PathBuf::from("/sys/devices/first"),
            PathBuf::from("/sys/devices/second"),
        ]
        .into_iter();

        assert_eq!(
            resolve_device_arg_with("2", || devices.next()).as_deref(),
            Some(Path::new("/sys/devices/second"))
        );
    }

    #[test]
    fn ipc_mode_selects_disabled_explicit_and_auto_paths() {
        assert_eq!(ipc_path_for_mode(&IpcMode::Disabled, None).unwrap(), None);
        assert_eq!(
            ipc_path_for_mode(&IpcMode::Path(PathBuf::from("/tmp/custom.sock")), None,).unwrap(),
            Some(PathBuf::from("/tmp/custom.sock"))
        );
        assert_eq!(
            ipc_path_for_mode(&IpcMode::Auto, Some(Ok(PathBuf::from("/tmp/default.sock"))),)
                .unwrap(),
            Some(PathBuf::from("/tmp/default.sock"))
        );
        assert_eq!(
            ipc_path_for_mode(
                &IpcMode::Auto,
                Some(Err(wiiland_ipc::ClientError::RuntimeDirectoryMissing)),
            )
            .unwrap(),
            None
        );
    }
    #[test]
    fn auto_ipc_ignores_relative_runtime_directory() {
        assert_eq!(
            ipc_path_for_mode(
                &IpcMode::Auto,
                Some(Err(wiiland_ipc::ClientError::RuntimeDirectoryRelative(
                    PathBuf::from("runtime"),
                ))),
            )
            .unwrap(),
            None
        );
    }
}
