use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use wiiland_core::{
    AimActivation, AimMode, AimSource, Backend, Config, IrAimMapping, IrTracking, TraceConfig,
    TraceFilter,
};

#[cfg(windows)]
use std::io;
#[cfg(windows)]
use std::time::{Duration, Instant};
#[cfg(windows)]
use wiiland_core::calibration::CalibrationStats;
#[cfg(windows)]
use wiiland_hid::{
    BluetoothAddress, Device as BluetoothDevice, EventKind, Interface, InterfaceMask, Monitor,
    MonitorMode, PairingCancellation, PairingMethod as HidPairingMethod, enumerate_devices,
    enumerate_radios, pair_device,
};

/// Controls whether the daemon publishes its IPC server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IpcMode {
    Auto,
    Disabled,
    Path(PathBuf),
}

/// The mutually-exclusive top-level operations accepted by `wiilandd`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Run,
    Help,
    Version,
    List,
    AxisMap,
    ValidationChecklist,
    Doctor,
    DumpConfig,
    CheckConfig,
    SelfTest,
    CalibrateAim,
    Pair,
}
/// Legacy PIN method selected when pairing a Wii Remote on Windows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PairingMode {
    RedSyncButton,
    OneAndTwoButtons,
}
impl Action {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Help => "--help",
            Self::Version => "--version",
            Self::List => "--list",
            Self::AxisMap => "--axis-map",
            Self::ValidationChecklist => "--validation-checklist",
            Self::Doctor => "--doctor",
            Self::DumpConfig => "--dump-config",
            Self::CheckConfig => "--check-config",
            Self::SelfTest => "--self-test",
            Self::CalibrateAim => "--calibrate-aim",
            Self::Pair => "--pair",
        }
    }
    fn diagnostic_without_config(self) -> bool {
        matches!(
            self,
            Self::Help
                | Self::Version
                | Self::List
                | Self::AxisMap
                | Self::ValidationChecklist
                | Self::Pair
        )
    }
}

/// Results of the first pass. This pass discovers config layering, IPC mode,
/// and operations; all other value options are deliberately deferred until
/// after config.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pass1 {
    pub config_path: Option<PathBuf>,
    pub explicit_config: bool,
    pub no_config: bool,
    pub ipc: IpcMode,
    pub action: Action,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cli {
    pub action: Action,
    pub config: Config,
    pub config_path: Option<PathBuf>,
    pub explicit_config: bool,
    pub no_config: bool,
    pub ipc: IpcMode,
    pub device: Option<String>,
    pub radio: Option<String>,
    pub pairing_mode: PairingMode,
    pub dry_run: bool,
    pub verbose: bool,
    pub trace: TraceConfig,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliError {
    pub code: i32,
    pub message: String,
    pub usage: bool,
}
impl CliError {
    fn syntax(message: impl Into<String>) -> Self {
        Self {
            code: libc::EINVAL,
            message: message.into(),
            usage: true,
        }
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self {
            code: libc::EINVAL,
            message: message.into(),
            usage: false,
        }
    }
}
impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for CliError {}

fn arg_strings<I, S>(args: I) -> Result<Vec<String>, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    args.into_iter()
        .map(|a| {
            a.into()
                .into_string()
                .map_err(|_| CliError::syntax("arguments must be UTF-8"))
        })
        .collect()
}

fn requested_action(
    selected: &mut Action,
    requested: Action,
    option: &str,
) -> Result<(), CliError> {
    if *selected != Action::Run && *selected != requested {
        return Err(CliError::conflict(format!(
            "wiilandd: conflicting actions: {} and {}",
            selected.label(),
            option
        )));
    }
    *selected = requested;
    Ok(())
}

fn ipc_path(value: &str, option: &str) -> Result<PathBuf, CliError> {
    if value.is_empty() {
        return Err(CliError::syntax(format!(
            "wiilandd: {} requires a path",
            option
        )));
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(CliError::syntax(format!(
            "wiilandd: {} requires an absolute path",
            option
        )));
    }
    Ok(path)
}

impl Cli {
    /// Parse the config selectors, IPC mode, and action hints without
    /// applying any value. Value options are deliberately deferred until
    /// after config.
    pub fn parse_pass1(args: &[String]) -> Result<Pass1, CliError> {
        let mut config_path = None;
        let mut explicit_config = false;
        let mut no_config = false;
        let mut ipc = IpcMode::Auto;
        let mut action = Action::Run;
        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_str();
            if let Some(path) = arg.strip_prefix("--config=") {
                if path.is_empty() {
                    return Err(CliError::syntax("wiilandd: --config requires a path"));
                }
                config_path = Some(PathBuf::from(path));
                explicit_config = true;
            } else if arg == "-c" || arg == "--config" {
                i += 1;
                let Some(path) = args.get(i) else {
                    return Err(CliError::syntax("wiilandd: --config requires a path"));
                };
                config_path = Some(PathBuf::from(path));
                explicit_config = true;
            } else if arg == "--no-config" {
                no_config = true;
            } else if arg == "--no-ipc" {
                if matches!(&ipc, IpcMode::Path(_)) {
                    return Err(CliError::conflict(
                        "wiilandd: --no-ipc cannot be combined with --ipc-socket",
                    ));
                }
                ipc = IpcMode::Disabled;
            } else if let Some(path) = arg.strip_prefix("--ipc-socket=") {
                if matches!(&ipc, IpcMode::Disabled) {
                    return Err(CliError::conflict(
                        "wiilandd: --no-ipc cannot be combined with --ipc-socket",
                    ));
                }
                ipc = IpcMode::Path(ipc_path(path, "--ipc-socket")?);
            } else if arg == "--ipc-socket" {
                i += 1;
                let Some(path) = args.get(i) else {
                    return Err(CliError::syntax("wiilandd: --ipc-socket requires a path"));
                };
                if matches!(&ipc, IpcMode::Disabled) {
                    return Err(CliError::conflict(
                        "wiilandd: --no-ipc cannot be combined with --ipc-socket",
                    ));
                }
                ipc = IpcMode::Path(ipc_path(path, "--ipc-socket")?);
            } else if arg == "--radio" || arg == "--pairing-method" {
                i += 1;
            } else if arg == "-h" || arg == "--help" {
                requested_action(&mut action, Action::Help, arg)?;
            } else if arg == "--version" {
                requested_action(&mut action, Action::Version, arg)?;
            } else if arg == "-l" || arg == "--list" {
                requested_action(&mut action, Action::List, arg)?;
            } else if arg == "--axis-map" {
                requested_action(&mut action, Action::AxisMap, arg)?;
            } else if arg == "--validation-checklist" {
                requested_action(&mut action, Action::ValidationChecklist, arg)?;
            } else if arg == "--doctor" {
                requested_action(&mut action, Action::Doctor, arg)?;
            } else if arg == "--dump-config" {
                requested_action(&mut action, Action::DumpConfig, arg)?;
            } else if arg == "--check-config" {
                requested_action(&mut action, Action::CheckConfig, arg)?;
            } else if arg == "--self-test" {
                requested_action(&mut action, Action::SelfTest, arg)?;
            } else if arg == "--calibrate-aim" {
                requested_action(&mut action, Action::CalibrateAim, arg)?;
            } else if arg == "--pair" {
                requested_action(&mut action, Action::Pair, arg)?;
            }
            i += 1;
        }
        if no_config && explicit_config {
            return Err(CliError::conflict(
                "wiilandd: --no-config cannot be combined with --config",
            ));
        }
        Ok(Pass1 {
            config_path,
            explicit_config,
            no_config,
            ipc,
            action,
        })
    }
}

impl Cli {
    /// Parse all options over an already-loaded configuration. Since this is
    /// always the second pass, command-line values necessarily win.
    pub fn parse_pass2(args: &[String], config: Config, pass1: Pass1) -> Result<Self, CliError> {
        let mut out = Self {
            action: Action::Run,
            config: {
                config
                    .validate()
                    .map_err(|e| CliError::syntax(e.to_string()))?;
                config
            },
            config_path: pass1.config_path.clone(),
            explicit_config: pass1.explicit_config,
            no_config: pass1.no_config,
            ipc: pass1.ipc.clone(),
            device: None,
            radio: None,
            pairing_mode: PairingMode::RedSyncButton,
            dry_run: false,
            verbose: false,
            trace: TraceConfig::default(),
        };
        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_str();
            macro_rules! value {
                () => {{
                    i += 1;
                    args.get(i).ok_or_else(|| {
                        CliError::syntax(format!("wiilandd: {} requires a value", arg))
                    })?
                }};
            }
            macro_rules! set_int {
                ($field:ident, $min:expr, $max:expr) => {{
                    let v = value!()
                        .parse::<i64>()
                        .ok()
                        .filter(|v| ($min..=$max).contains(v))
                        .ok_or_else(|| {
                            CliError::syntax(format!("wiilandd: invalid value for {}", arg))
                        })?;
                    out.config.$field = v as i32;
                }};
            }
            if arg == "-h" || arg == "--help" {
                requested_action(&mut out.action, Action::Help, arg)?;
            } else if arg == "--version" {
                requested_action(&mut out.action, Action::Version, arg)?;
            } else if arg == "-l" || arg == "--list" {
                requested_action(&mut out.action, Action::List, arg)?;
            } else if arg == "--axis-map" {
                requested_action(&mut out.action, Action::AxisMap, arg)?;
            } else if arg == "--validation-checklist" {
                requested_action(&mut out.action, Action::ValidationChecklist, arg)?;
            } else if arg == "--doctor" {
                requested_action(&mut out.action, Action::Doctor, arg)?;
            } else if arg == "--dump-config" {
                requested_action(&mut out.action, Action::DumpConfig, arg)?;
            } else if arg == "--check-config" {
                requested_action(&mut out.action, Action::CheckConfig, arg)?;
            } else if arg == "--self-test" {
                requested_action(&mut out.action, Action::SelfTest, arg)?;
            } else if arg == "--calibrate-aim" {
                requested_action(&mut out.action, Action::CalibrateAim, arg)?;
            } else if arg == "--pair" {
                requested_action(&mut out.action, Action::Pair, arg)?;
            } else if arg == "--no-config" {
            } else if arg == "--config" {
                i += 1;
            } else if arg.starts_with("--config=")
                || arg == "--no-ipc"
                || arg.starts_with("--ipc-socket=")
            {
            } else if arg == "--ipc-socket" {
                i += 1;
            } else if arg == "-v" || arg == "--verbose" {
                out.verbose = true;
            } else if arg == "-n" || arg == "--dry-run" {
                out.dry_run = true;
            } else if arg == "-d" || arg == "--device" {
                out.device = Some(value!().to_owned());
            } else if let Some(v) = arg.strip_prefix("--device=") {
                if v.is_empty() {
                    return Err(CliError::syntax("wiilandd: --device requires a value"));
                }
                out.device = Some(v.to_owned());
            } else if arg == "--radio" {
                let value = value!();
                if value.is_empty() {
                    return Err(CliError::syntax("wiilandd: --radio requires a value"));
                }
                out.radio = Some(value.to_owned());
            } else if let Some(value) = arg.strip_prefix("--radio=") {
                if value.is_empty() {
                    return Err(CliError::syntax("wiilandd: --radio requires a value"));
                }
                out.radio = Some(value.to_owned());
            } else if arg == "--pairing-method" {
                out.pairing_mode = parse_pairing_mode(value!())?;
            } else if let Some(value) = arg.strip_prefix("--pairing-method=") {
                out.pairing_mode = parse_pairing_mode(value)?;
            } else if arg == "-p" || arg == "--profile" {
                let v = value!();
                out.config.profile = wiiland_core::Profile::parse(v).ok_or_else(|| {
                    CliError::syntax(format!("wiilandd: invalid value for {}", arg))
                })?;
            } else if let Some(v) = arg.strip_prefix("--profile=") {
                out.config.profile = wiiland_core::Profile::parse(v).ok_or_else(|| {
                    CliError::syntax(format!("wiilandd: invalid value for {}", arg))
                })?;
            } else if arg == "--backend" {
                out.config.backend = parse_backend(value!())?;
            } else if let Some(value) = arg.strip_prefix("--backend=") {
                out.config.backend = parse_backend(value)?;
            } else if let Some(v) = arg.strip_prefix("--trace-events=") {
                out.trace.enabled = true;
                out.trace.filter = v
                    .parse()
                    .map_err(|_| CliError::syntax("wiilandd: invalid value for --trace-events"))?;
            } else if arg == "--trace-events" {
                out.trace.enabled = true;
                out.trace.filter = TraceFilter::All;
            } else if arg == "--pointer-speed" {
                set_int!(pointer_speed, 1i64, 127i64);
            } else if let Some(v) = arg.strip_prefix("--pointer-speed=") {
                out.config.pointer_speed = v
                    .parse::<i64>()
                    .ok()
                    .filter(|v| (1..=127).contains(v))
                    .ok_or_else(|| {
                    CliError::syntax("wiilandd: invalid value for --pointer-speed")
                })? as i32;
            } else if arg == "--ir-speed" {
                set_int!(ir_speed, 1i64, 127i64);
            } else if let Some(v) = arg.strip_prefix("--ir-speed=") {
                out.config.ir_speed = v
                    .parse::<i64>()
                    .ok()
                    .filter(|v| (1..=127).contains(v))
                    .ok_or_else(|| CliError::syntax("wiilandd: invalid value for --ir-speed"))?
                    as i32;
            } else if arg == "--ir-deadzone" {
                set_int!(ir_deadzone, 0i64, 127i64);
            } else if let Some(v) = arg.strip_prefix("--ir-deadzone=") {
                out.config.ir_deadzone = v
                    .parse::<i64>()
                    .ok()
                    .filter(|v| (0..=127).contains(v))
                    .ok_or_else(|| CliError::syntax("wiilandd: invalid value for --ir-deadzone"))?
                    as i32;
            } else if arg == "--ir-smoothing" {
                set_int!(ir_smoothing, 0i64, 95i64);
            } else if let Some(v) = arg.strip_prefix("--ir-smoothing=") {
                out.config.ir_smoothing = v
                    .parse::<i64>()
                    .ok()
                    .filter(|v| (0..=95).contains(v))
                    .ok_or_else(|| CliError::syntax("wiilandd: invalid value for --ir-smoothing"))?
                    as i32;
            } else if arg == "--ir-tracking" {
                out.config.ir_tracking = value!().parse_choice(IrTracking::parse, arg)?;
            } else if let Some(v) = arg.strip_prefix("--ir-tracking=") {
                out.config.ir_tracking = parse_choice(v, IrTracking::parse, arg)?;
            } else if arg == "--ir-aim-mapping" {
                out.config.ir_aim_mapping = value!().parse_choice(IrAimMapping::parse, arg)?;
            } else if let Some(v) = arg.strip_prefix("--ir-aim-mapping=") {
                out.config.ir_aim_mapping = parse_choice(v, IrAimMapping::parse, arg)?;
            } else if arg == "--aim-mode" {
                out.config.aim_mode = value!().parse_choice(AimMode::parse, arg)?;
            } else if let Some(v) = arg.strip_prefix("--aim-mode=") {
                out.config.aim_mode = parse_choice(v, AimMode::parse, arg)?;
            } else if arg == "--aim-source" {
                out.config.aim_source = value!().parse_choice(AimSource::parse, arg)?;
            } else if let Some(v) = arg.strip_prefix("--aim-source=") {
                out.config.aim_source = parse_choice(v, AimSource::parse, arg)?;
            } else if arg == "--aim-activation" {
                out.config.aim_activation = value!().parse_choice(AimActivation::parse, arg)?;
            } else if let Some(v) = arg.strip_prefix("--aim-activation=") {
                out.config.aim_activation = parse_choice(v, AimActivation::parse, arg)?;
            } else if arg == "--aim-sensitivity" {
                set_int!(aim_sensitivity, 1i64, 127i64);
            } else if let Some(v) = arg.strip_prefix("--aim-sensitivity=") {
                out.config.aim_sensitivity = int_value(v, 1, 127, arg)?;
            } else if arg == "--aim-deadzone" {
                set_int!(aim_deadzone, 0i64, 32767i64);
            } else if let Some(v) = arg.strip_prefix("--aim-deadzone=") {
                out.config.aim_deadzone = int_value(v, 0, 32767, arg)?;
            } else if arg == "--aim-smoothing" {
                set_int!(aim_smoothing, 0i64, 95i64);
            } else if let Some(v) = arg.strip_prefix("--aim-smoothing=") {
                out.config.aim_smoothing = int_value(v, 0, 95, arg)?;
            } else if arg == "--aim-invert-x" {
                out.config.aim_invert_x = bool_value(value!(), arg)?;
            } else if let Some(v) = arg.strip_prefix("--aim-invert-x=") {
                out.config.aim_invert_x = bool_value(v, arg)?;
            } else if arg == "--aim-invert-y" {
                out.config.aim_invert_y = bool_value(value!(), arg)?;
            } else if let Some(v) = arg.strip_prefix("--aim-invert-y=") {
                out.config.aim_invert_y = bool_value(v, arg)?;
            } else if arg == "--aim-calibration-duration" {
                set_int!(aim_calibration_duration, 1i64, 30i64);
            } else if let Some(v) = arg.strip_prefix("--aim-calibration-duration=") {
                out.config.aim_calibration_duration = int_value(v, 1, 30, arg)?;
            } else if arg == "--config" || arg == "-c" {
                i += 1;
            } else {
                return Err(CliError::syntax(format!(
                    "wiilandd: unrecognized option '{}'",
                    arg
                )));
            }
            i += 1;
        }
        out.config
            .validate()
            .map_err(|e| CliError::syntax(e.to_string()))?;
        out.config
            .backend
            .resolve_for_current_platform()
            .map_err(|error| CliError::conflict(format!("wiilandd: {error}")))?;
        Ok(out)
    }

    pub fn parse<I, S>(args: I) -> Result<Self, CliError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut args = arg_strings(args)?;
        if !args.is_empty() {
            args.remove(0);
        }
        let pass1 = Self::parse_pass1(&args)?;
        let config = if pass1.no_config || pass1.action.diagnostic_without_config() {
            Config::default()
        } else if let Some(path) = pass1.config_path.as_ref() {
            Config::load_file(path).map_err(|e| CliError::syntax(e.to_string()))?
        } else {
            Config::load_default_layers().map_err(|e| CliError::syntax(e.to_string()))?
        };
        Self::parse_pass2(&args, config, pass1)
    }
}

fn parse_choice<T>(
    value: &str,
    parser: impl Fn(&str) -> Option<T>,
    option: &str,
) -> Result<T, CliError> {
    parser(value).ok_or_else(|| CliError::syntax(format!("wiilandd: invalid value for {}", option)))
}
trait ParseChoice {
    fn parse_choice<T>(
        &self,
        parser: impl Fn(&str) -> Option<T>,
        option: &str,
    ) -> Result<T, CliError>;
}
impl ParseChoice for str {
    fn parse_choice<T>(
        &self,
        parser: impl Fn(&str) -> Option<T>,
        option: &str,
    ) -> Result<T, CliError> {
        parse_choice(self, parser, option)
    }
}
fn int_value(value: &str, min: i64, max: i64, option: &str) -> Result<i32, CliError> {
    value
        .parse::<i64>()
        .ok()
        .filter(|v| (min..=max).contains(v))
        .map(|v| v as i32)
        .ok_or_else(|| CliError::syntax(format!("wiilandd: invalid value for {}", option)))
}
fn bool_value(value: &str, option: &str) -> Result<bool, CliError> {
    match value {
        "yes" | "true" | "1" => Ok(true),
        "no" | "false" | "0" => Ok(false),
        _ => Err(CliError::syntax(format!(
            "wiilandd: invalid value for {}",
            option
        ))),
    }
}

fn parse_backend(value: &str) -> Result<Backend, CliError> {
    match value {
        "auto" => Ok(Backend::Auto),
        "uinput" => Ok(Backend::Uinput),
        "windows-vhf" => Ok(Backend::WindowsVhf),
        _ => Err(CliError::syntax("wiilandd: invalid value for --backend")),
    }
}

fn parse_pairing_mode(value: &str) -> Result<PairingMode, CliError> {
    match value {
        "sync" => Ok(PairingMode::RedSyncButton),
        "1+2" => Ok(PairingMode::OneAndTwoButtons),
        _ => Err(CliError::syntax(
            "wiilandd: --pairing-method must be sync or 1+2",
        )),
    }
}

#[cfg(not(windows))]
pub fn usage() -> &'static str {
    "Usage:\n\twiilandd [OPTIONS]\n\twiilandd --device <number|/sys/path> [OPTIONS]\n\nOptions:\n\t-h, --help       Show this help\n\t    --version    Show version\n\t-l, --list       List connected Wii Remote devices and exit\n\t                 Combine with --verbose for devtype/extension\n\t-d, --device     Bridge one device instead of monitoring all devices\n\t-p, --profile    gamepad, desktop, or both (default: gamepad)\n\t    --backend <uinput>       Input backend (default: uinput)\n\t    --ir-speed <1-127>       IR pointer gain (default: 8)\n\t    --ir-deadzone <0-127>   IR jitter deadzone (default: 0)\n\t    --ir-smoothing <0-95>   IR smoothing percent (default: 0)\n\t    --ir-tracking <dual|centroid|first>\n\t    --ir-aim-mapping <relative|absolute>\n\t    --pointer-speed <1-127>  Desktop pointer step (default: 16)\n\t    --aim-mode <off|mouse|right-stick>\n\t    --aim-source <auto|ir|motion-plus|accelerometer>\n\t    --aim-activation <always|b|z|c>\n\t    --aim-sensitivity <1-127>\n\t    --aim-deadzone <0-32767>\n\t    --aim-smoothing <0-95>\n\t    --aim-invert-x <yes|no>\n\t    --aim-invert-y <yes|no>\n\t    --calibrate-aim\n\t    --aim-calibration-duration <1-30>\n\t-c, --config     Load key=value config file\n\t    --no-config   Do not load the default config file\n\t    --no-ipc      Disable the IPC server\n\t    --ipc-socket PATH\n\t                 Bind the IPC server to an explicit absolute path\n\t-n, --dry-run    Do not create /dev/uinput devices or emit input\n\t    --check-config  Validate configuration and exit\n\t    --self-test  Run deterministic self tests and exit\n\t    --trace-events[=all|keys|axes|ir|motion-plus]\n\t    --axis-map   Print virtual gamepad axis mapping and exit\n\t    --validation-checklist  Print required hardware validation matrix\n\t    --doctor    Print runtime readiness diagnostics and exit\n\t    --dump-config  Print resolved configuration and exit\n\t-v, --verbose    Print device lifecycle details\n\nwiilandd creates Linux uinput virtual input devices named\n\"WiiLand Virtual Controller\" and \"WiiLand Virtual Desktop\"\nthrough the common evdev/libinput input stack.\n"
}

#[cfg(windows)]
pub fn usage() -> &'static str {
    "Usage:\n\twiilandd [OPTIONS]\n\twiilandd --device <ordinal|HID identity> [OPTIONS]\n\nOptions:\n\t-h, --help       Show this help\n\t    --version    Show version\n\t-l, --list       List connected Wii Remote HID identities\n\t-d, --device     Select an HID ordinal/identity; with --pair use a Bluetooth address or inquiry ordinal\n\t-p, --profile    gamepad, desktop, or both (default: gamepad)\n\t    --backend <auto|windows-vhf|uinput> Output backend; uinput is rejected on Windows\n\t    --pair        Pair a discovered Wii Remote using Windows Bluetooth\n\t    --radio <address|ordinal> Select local Bluetooth radio (automatic when one radio exists)\n\t    --pairing-method <sync|1+2> Pairing PIN method (default: sync)\n\t    --calibrate-aim  Capture motion sensor calibration from one device\n\t    --aim-calibration-duration <1-30>\n\t-c, --config     Load key=value config file\n\t    --no-config   Do not load the default config file\n\t    --no-ipc      Disable the IPC server\n\t    --ipc-socket PATH\n\t                 Bind the IPC server to an explicit absolute path\n\t-n, --dry-run    Do not emit virtual input\n\t    --check-config  Validate configuration and exit\n\t    --self-test  Run deterministic core checks and exit\n\t    --trace-events[=all|keys|axes|ir|motion-plus]\n\t    --axis-map   Describe the Windows HID report mapping\n\t    --validation-checklist  Print Windows validation matrix\n\t    --doctor    Print Windows backend and runtime dependency diagnostics\n\t    --dump-config  Print resolved configuration and exit\n\t-v, --verbose    Print additional Windows HID diagnostics\n\nVirtual output requires the wiiland-output-service and the wiiland-vhid VHF driver.\n"
}
/// Process the actual command line and return a process exit status.
#[cfg(not(windows))]
pub fn run() -> i32 {
    let args: Vec<OsString> = std::env::args_os().collect();
    let cli = match Cli::parse(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", e);
            if e.usage {
                eprint!("{}", usage());
            }
            return e.code;
        }
    };
    match crate::commands::execute(&cli) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{}", e);
            e.code()
        }
    }
}

#[cfg(windows)]
pub fn run() -> i32 {
    let args: Vec<OsString> = std::env::args_os().collect();
    let cli = match Cli::parse(args) {
        Ok(cli) => cli,
        Err(error) => {
            eprintln!("{error}");
            if error.usage {
                eprint!("{}", usage());
            }
            return error.code;
        }
    };
    execute_windows(&cli)
}
#[cfg(windows)]
#[derive(Debug)]
struct WindowsCommandError {
    code: i32,
    message: String,
}

#[cfg(windows)]
impl WindowsCommandError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[cfg(windows)]
fn execute_windows(cli: &Cli) -> i32 {
    let result = match cli.action {
        Action::Help => {
            print!("{}", usage());
            Ok(())
        }
        Action::Version => {
            println!("wiilandd {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Action::List => windows_list_devices(cli.verbose),
        Action::AxisMap => {
            println!("axis-map=Windows HID report mapping; Linux evdev codes do not apply");
            Ok(())
        }
        Action::ValidationChecklist => {
            println!(
                "windows.physical-hid=required\nwindows.bluetooth-pairing=required\nwindows.vhf-gamepad=required\nwindows.desktop-sendinput=required\nwindows.output-service=required"
            );
            Ok(())
        }
        Action::Doctor => windows_doctor(&cli.config),
        Action::DumpConfig => {
            print!("{}", cli.config.dump());
            Ok(())
        }
        Action::CheckConfig => windows_check_config(&cli.config),
        Action::SelfTest => windows_self_test(),
        Action::CalibrateAim => windows_calibrate_aim(cli),
        Action::Pair => windows_pair_device(cli),
        Action::Run => return crate::windows_runtime::run_cli(cli),
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("wiilandd: {}", error.message);
            error.code
        }
    }
}

#[cfg(windows)]
fn windows_error_code(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO).unsigned_abs() as i32
}

#[cfg(windows)]
fn windows_list_devices(verbose: bool) -> Result<(), WindowsCommandError> {
    let mut monitor = Monitor::new(MonitorMode::Enumerate).map_err(|error| {
        WindowsCommandError::new(
            windows_error_code(&error),
            format!("cannot enumerate Windows Wii Remote HID devices: {error}"),
        )
    })?;
    let mut count = 0usize;
    loop {
        let path = monitor.poll().map_err(|error| {
            WindowsCommandError::new(
                windows_error_code(&error),
                format!("Windows HID enumeration failed: {error}"),
            )
        })?;
        let Some(path) = path else {
            break;
        };
        count += 1;
        println!("{count}\t{}", path.display());
        if verbose {
            println!("\tmetadata=not-exposed-by-Windows-HID-enumerator");
        }
    }
    if count == 0 {
        println!("No Wii Remote devices found");
    }
    Ok(())
}

#[cfg(windows)]
pub fn resolve_device_arg(arg: &str) -> Option<PathBuf> {
    if arg.starts_with("\\\\?\\") || arg.starts_with("\\\\.\\") {
        return Some(PathBuf::from(arg));
    }
    let number = arg.parse::<usize>().ok().filter(|number| *number != 0)?;
    let mut monitor = Monitor::new(MonitorMode::Enumerate).ok()?;
    for index in 1..=number {
        let path = monitor.poll().ok().flatten()?;
        if index == number {
            return Some(path);
        }
    }
    None
}

#[cfg(windows)]
fn windows_doctor(config: &Config) -> Result<(), WindowsCommandError> {
    let resolved = config
        .backend
        .resolve_for_current_platform()
        .map(|backend| backend.as_str())
        .unwrap_or("unsupported");
    println!(
        "platform=windows\nbackend.requested={}\nbackend.resolved={}\nprofile={}\naim.mode={}\naim.source={}\naim.activation={}\nruntime.output.service=wiiland-output-service\nruntime.output.driver=wiiland-vhid\nruntime.output.installation=not-probed\nruntime.output.probe=not-performed",
        config.backend.as_str(),
        resolved,
        config.profile.as_str().unwrap_or("unknown"),
        config.aim_mode.as_str(),
        config.aim_source.as_str(),
        config.aim_activation.as_str()
    );
    Ok(())
}

#[cfg(windows)]
fn windows_check_config(config: &Config) -> Result<(), WindowsCommandError> {
    let backend = config
        .backend
        .resolve_for_current_platform()
        .map_err(|error| WindowsCommandError::new(libc::EINVAL, error))?;
    println!(
        "config.status=ok\nbackend.requested={}\nbackend.resolved={}\nruntime.output.service=wiiland-output-service\nruntime.output.driver=wiiland-vhid\nruntime.output.probe=not-performed",
        config.backend.as_str(),
        backend.as_str()
    );
    Ok(())
}

#[cfg(windows)]
fn windows_self_test() -> Result<(), WindowsCommandError> {
    if wiiland_core::mapping::scale_signed_axis(-500, 500, 500)
        != wiiland_core::mapping::VIRTUAL_AXIS_MIN
        || wiiland_core::mapping::scale_signed_axis(500, 500, 500)
            != wiiland_core::mapping::VIRTUAL_AXIS_MAX
        || wiiland_core::mapping::map_key(4) != Some(wiiland_core::mapping::BTN_SOUTH)
    {
        return Err(WindowsCommandError::new(
            libc::EINVAL,
            "self-test failed: core input mapping contract",
        ));
    }
    println!("wiilandd self-test: ok");
    Ok(())
}

#[cfg(windows)]
fn windows_calibrate_aim(cli: &Cli) -> Result<(), WindowsCommandError> {
    let path = cli
        .device
        .as_deref()
        .and_then(resolve_device_arg)
        .or_else(|| resolve_device_arg("1"))
        .ok_or_else(|| {
            WindowsCommandError::new(
                libc::ENODEV,
                "cannot resolve calibration device; run --list and pass --device <list ordinal|opaque HID identity>",
            )
        })?;
    let mut interface = Interface::new(&path).map_err(|error| {
        WindowsCommandError::new(
            windows_error_code(&error),
            format!("cannot open HID identity {}: {error}", path.display()),
        )
    })?;
    let available = interface.available() & (InterfaceMask::ACCEL | InterfaceMask::MOTION_PLUS);
    if available.is_empty() {
        return Err(WindowsCommandError::new(
            libc::ENODEV,
            "calibration device has no available accelerometer or MotionPlus sensor",
        ));
    }
    let opened_result = interface.open(available);
    let opened = match &opened_result {
        Ok(mask) => *mask,
        Err(error) => error.opened(),
    } & available;
    if opened.is_empty() {
        return Err(WindowsCommandError::new(
            libc::ENODEV,
            "cannot open calibration sensor interfaces",
        ));
    }
    if let Err(error) = opened_result {
        eprintln!("wiilandd: warning: some calibration sensors unavailable: {error}");
    }
    eprintln!(
        "wiilandd: place the Wii Remote face down on a stable surface and keep it still for {} seconds",
        cli.config.aim_calibration_duration
    );
    let deadline = Instant::now() + Duration::from_secs(cli.config.aim_calibration_duration as u64);
    let mut accelerometer = CalibrationStats::new();
    let mut motion_plus = CalibrationStats::new();
    while Instant::now() < deadline {
        match interface.dispatch() {
            Ok(event) => {
                windows_collect_calibration_event(event.kind, &mut accelerometer, &mut motion_plus)?
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                return Err(WindowsCommandError::new(
                    windows_error_code(&error),
                    format!("calibration HID read failed: {error}"),
                ));
            }
        }
    }
    let accel = accelerometer.finish();
    let motion = motion_plus.finish();
    if accel.is_none() && motion.is_none() {
        return Err(WindowsCommandError::new(
            libc::EAGAIN,
            "calibration failed: no stable accelerometer or MotionPlus sample window was captured",
        ));
    }
    println!("# WiiLand motion aim calibration");
    println!(
        "# samples.accelerometer={} jitter.accelerometer={}",
        accelerometer.samples,
        accelerometer.jitter()
    );
    println!(
        "# samples.motion-plus={} jitter.motion-plus={}",
        motion_plus.samples,
        motion_plus.jitter()
    );
    if let Some(calibration) = accel {
        windows_print_calibration("aim-accel-zero", calibration);
    }
    if let Some(calibration) = motion {
        windows_print_calibration("aim-motion-plus-bias", calibration);
    }
    Ok(())
}

#[cfg(windows)]
fn windows_collect_calibration_event(
    event: EventKind,
    accelerometer: &mut CalibrationStats,
    motion_plus: &mut CalibrationStats,
) -> Result<(), WindowsCommandError> {
    match event {
        EventKind::Accel(value) => accelerometer.add([value.x, value.y, value.z]),
        EventKind::MotionPlus(value) => motion_plus.add([value.x, value.y, value.z]),
        EventKind::Watch | EventKind::Gone => {
            return Err(WindowsCommandError::new(
                libc::ENODEV,
                "calibration interrupted because the Wii Remote disconnected or changed",
            ));
        }
        _ => {}
    }
    Ok(())
}

#[cfg(windows)]
fn windows_print_calibration(prefix: &str, calibration: wiiland_core::SensorCalibration) {
    println!(
        "{}-x={}\n{}-y={}\n{}-z={}",
        prefix, calibration.x, prefix, calibration.y, prefix, calibration.z
    );
}

#[cfg(windows)]
fn windows_pair_device(cli: &Cli) -> Result<(), WindowsCommandError> {
    let device_selector = cli.device.as_deref().ok_or_else(|| {
        WindowsCommandError::new(
            libc::EINVAL,
            "--pair requires --device <Bluetooth address|one-based ordinal>",
        )
    })?;
    let radios = enumerate_radios().map_err(|error| {
        WindowsCommandError::new(
            libc::EIO,
            format!("cannot enumerate Windows Bluetooth radios: {error}"),
        )
    })?;
    if radios.is_empty() {
        return Err(WindowsCommandError::new(
            libc::ENODEV,
            "no Windows Bluetooth radio is available",
        ));
    }
    let radio = match cli.radio.as_deref() {
        Some(selector) => windows_select_radio(&radios, selector)?,
        None if radios.len() == 1 => &radios[0],
        None => {
            for (index, radio) in radios.iter().enumerate() {
                println!("radio.{}\t{}\t{}", index + 1, radio.address, radio.name);
            }
            return Err(WindowsCommandError::new(
                libc::EINVAL,
                "multiple Bluetooth radios are available; select one with --radio <address|ordinal>",
            ));
        }
    };
    let devices = enumerate_devices(radio.address, 8).map_err(|error| {
        WindowsCommandError::new(
            libc::EIO,
            format!(
                "Bluetooth inquiry failed on radio {}: {error}",
                radio.address
            ),
        )
    })?;
    for (index, device) in devices.iter().enumerate() {
        println!(
            "device.{}\t{}\t{}\tconnected={}\tremembered={}\tauthenticated={}",
            index + 1,
            device.address,
            device.name,
            device.connected,
            device.remembered,
            device.authenticated
        );
    }
    let device = windows_select_bluetooth_device(&devices, device_selector)?;
    let method = match cli.pairing_mode {
        PairingMode::RedSyncButton => HidPairingMethod::RedSyncButton,
        PairingMode::OneAndTwoButtons => HidPairingMethod::OneAndTwoButtons,
    };
    let paired = pair_device(
        radio.address,
        device.address,
        method,
        &PairingCancellation::new(),
    )
    .map_err(|error| WindowsCommandError::new(libc::EIO, format!("pairing failed: {error}")))?;
    println!(
        "pair.status=ok\npair.radio={}\npair.device={}\npair.already-authenticated={}\npair.hid-service-enabled={}",
        radio.address, paired.device, paired.was_already_authenticated, paired.hid_service_enabled
    );
    Ok(())
}

#[cfg(windows)]
fn windows_parse_bluetooth_address(value: &str) -> Option<BluetoothAddress> {
    let mut bytes = [0; 6];
    let mut parts = value.split([':', '-']);
    for byte in &mut bytes {
        let part = parts.next()?;
        if part.len() != 2 {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    parts
        .next()
        .is_none()
        .then(|| BluetoothAddress::from_bytes(bytes))
}

#[cfg(windows)]
fn windows_select_radio<'a>(
    radios: &'a [wiiland_hid::Radio],
    selector: &str,
) -> Result<&'a wiiland_hid::Radio, WindowsCommandError> {
    windows_parse_bluetooth_address(selector)
        .and_then(|address| radios.iter().find(|radio| radio.address == address))
        .or_else(|| {
            selector
                .parse::<usize>()
                .ok()
                .filter(|index| *index > 0)
                .and_then(|index| radios.get(index - 1))
        })
        .ok_or_else(|| {
            WindowsCommandError::new(
                libc::EINVAL,
                format!("unknown Bluetooth radio selector '{selector}'"),
            )
        })
}

#[cfg(windows)]
fn windows_select_bluetooth_device<'a>(
    devices: &'a [BluetoothDevice],
    selector: &str,
) -> Result<&'a BluetoothDevice, WindowsCommandError> {
    windows_parse_bluetooth_address(selector)
        .and_then(|address| devices.iter().find(|device| device.address == address))
        .or_else(|| {
            selector
                .parse::<usize>()
                .ok()
                .filter(|index| *index > 0)
                .and_then(|index| devices.get(index - 1))
        })
        .ok_or_else(|| {
            WindowsCommandError::new(
                libc::ENODEV,
                format!(
                    "Bluetooth device selector '{selector}' was not found in the inquiry results"
                ),
            )
        })
}
#[cfg(test)]
mod tests {
    use super::*;

    fn pass(args: &[&str]) -> Pass1 {
        let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        Cli::parse_pass1(&args).unwrap()
    }

    #[test]
    fn ipc_defaults_to_auto_and_is_independent_of_config() {
        let first = pass(&[]);
        let second = pass(&["--no-config"]);
        assert_eq!(first.ipc, IpcMode::Auto);
        assert_eq!(second.ipc, IpcMode::Auto);
        assert_ne!(first.no_config, second.no_config);
    }

    #[test]
    fn ipc_accepts_both_path_forms() {
        assert_eq!(pass(&["--no-ipc"]).ipc, IpcMode::Disabled);
        assert_eq!(
            pass(&["--ipc-socket", "/tmp/wiilandd.sock"]).ipc,
            IpcMode::Path(PathBuf::from("/tmp/wiilandd.sock"))
        );
        assert_eq!(
            pass(&["--ipc-socket=/tmp/wiilandd.sock"]).ipc,
            IpcMode::Path(PathBuf::from("/tmp/wiilandd.sock"))
        );
    }

    #[test]
    fn ipc_rejects_empty_and_relative_paths() {
        for args in [
            vec!["--ipc-socket", ""],
            vec!["--ipc-socket="],
            vec!["--ipc-socket", "wiilandd.sock"],
            vec!["--ipc-socket=wiilandd.sock"],
        ] {
            assert!(
                Cli::parse_pass1(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err()
            );
        }
    }

    #[test]
    fn ipc_rejects_no_ipc_conflicts() {
        assert!(
            Cli::parse_pass1(
                &["--no-ipc", "--ipc-socket", "/tmp/wiilandd.sock"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            )
            .is_err()
        );
        assert!(
            Cli::parse_pass1(
                &["--ipc-socket=/tmp/wiilandd.sock", "--no-ipc"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            )
            .is_err()
        );
    }

    #[test]
    fn ipc_socket_value_is_consumed_before_action_parsing() {
        let parsed = pass(&["--ipc-socket", "/tmp/--help", "--help"]);
        assert_eq!(parsed.action, Action::Help);
        assert_eq!(parsed.ipc, IpcMode::Path(PathBuf::from("/tmp/--help")));
    }

    #[test]
    fn pass2_preserves_ipc_without_touching_config() {
        let pass1 = pass(&["--ipc-socket=/tmp/wiilandd.sock"]);
        let config = Config::default();
        let parsed = Cli::parse_pass2(&[], config.clone(), pass1).unwrap();
        assert_eq!(
            parsed.ipc,
            IpcMode::Path(PathBuf::from("/tmp/wiilandd.sock"))
        );
        assert_eq!(parsed.config, config);
    }

    #[test]
    fn windows_pair_options_preserve_radio_device_and_method_selection() {
        let args = [
            "--pair",
            "--device",
            "2",
            "--radio",
            "00:11:22:33:44:55",
            "--pairing-method",
            "1+2",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        let pass1 = Cli::parse_pass1(&args).unwrap();
        let cli = Cli::parse_pass2(&args, Config::default(), pass1).unwrap();

        assert_eq!(cli.action, Action::Pair);
        assert_eq!(cli.device.as_deref(), Some("2"));
        assert_eq!(cli.radio.as_deref(), Some("00:11:22:33:44:55"));
        assert_eq!(cli.pairing_mode, PairingMode::OneAndTwoButtons);
    }

    #[test]
    fn backend_values_are_checked_against_the_current_platform() {
        for (value, backend) in [
            ("auto", Backend::Auto),
            ("uinput", Backend::Uinput),
            ("windows-vhf", Backend::WindowsVhf),
        ] {
            let args = vec![format!("--backend={value}")];
            let pass1 = Cli::parse_pass1(&args).unwrap();
            let result = Cli::parse_pass2(&args, Config::default(), pass1);
            assert_eq!(
                result.is_ok(),
                backend.resolve_for_current_platform().is_ok(),
                "backend {value}"
            );
            if let Ok(cli) = result {
                assert_eq!(cli.config.backend, backend);
            }
        }
    }
}
