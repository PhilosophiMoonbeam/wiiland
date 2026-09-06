use std::fmt;
use std::io;
use std::ops::{BitOr, BitOrAssign};
use std::path::{Path, PathBuf};

pub const SYSTEM_CONFIG_PATH: &str = "/etc/wiiland/wiilandd.conf";
pub const MAX_DEVICE_RULES: usize = 32;
pub const MAX_LINE_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    Uinput,
}
impl Backend {
    pub fn as_str(self) -> &'static str {
        "uinput"
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Profile(u8);
impl Profile {
    pub const GAMEPAD: Self = Self(1);
    pub const DESKTOP: Self = Self(2);
    pub const BOTH: Self = Self(3);
    pub const fn bits(self) -> u8 {
        self.0
    }
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn is_valid(self) -> bool {
        self.0 != 0 && self.0 <= Self::BOTH.0
    }
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            Self::GAMEPAD => Some("gamepad"),
            Self::DESKTOP => Some("desktop"),
            Self::BOTH => Some("both"),
            _ => None,
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "gamepad" => Some(Self::GAMEPAD),
            "desktop" => Some(Self::DESKTOP),
            "both" => Some(Self::BOTH),
            _ => None,
        }
    }
}
impl Default for Profile {
    fn default() -> Self {
        Self::GAMEPAD
    }
}
impl BitOr for Profile {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}
impl BitOrAssign for Profile {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrTracking {
    First,
    Centroid,
    Dual,
}
impl IrTracking {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::Centroid => "centroid",
            Self::Dual => "dual",
        }
    }
    pub fn parse(v: &str) -> Option<Self> {
        match v {
            "first" => Some(Self::First),
            "centroid" => Some(Self::Centroid),
            "dual" => Some(Self::Dual),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrAimMapping {
    Relative,
    Absolute,
}
impl IrAimMapping {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Relative => "relative",
            Self::Absolute => "absolute",
        }
    }
    pub fn parse(v: &str) -> Option<Self> {
        match v {
            "relative" => Some(Self::Relative),
            "absolute" => Some(Self::Absolute),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AimMode {
    Off,
    Mouse,
    RightStick,
}
impl AimMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Mouse => "mouse",
            Self::RightStick => "right-stick",
        }
    }
    pub fn parse(v: &str) -> Option<Self> {
        match v {
            "off" => Some(Self::Off),
            "mouse" => Some(Self::Mouse),
            "right-stick" => Some(Self::RightStick),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AimSource {
    Auto,
    Ir,
    MotionPlus,
    Accelerometer,
}
impl AimSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Ir => "ir",
            Self::MotionPlus => "motion-plus",
            Self::Accelerometer => "accelerometer",
        }
    }
    pub fn parse(v: &str) -> Option<Self> {
        match v {
            "auto" => Some(Self::Auto),
            "ir" => Some(Self::Ir),
            "motion-plus" => Some(Self::MotionPlus),
            "accelerometer" => Some(Self::Accelerometer),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AimActivation {
    Always,
    B,
    Z,
    C,
}
impl AimActivation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::B => "b",
            Self::Z => "z",
            Self::C => "c",
        }
    }
    pub fn parse(v: &str) -> Option<Self> {
        match v {
            "always" => Some(Self::Always),
            "b" => Some(Self::B),
            "z" => Some(Self::Z),
            "c" => Some(Self::C),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IrRectangle {
    pub left: i32,
    pub right: i32,
    pub top: i32,
    pub bottom: i32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SensorCalibration {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub axes: u8,
}
impl SensorCalibration {
    pub const X: u8 = 1;
    pub const Y: u8 = 2;
    pub const Z: u8 = 4;
    pub const ALL: u8 = 7;
    pub const fn complete(self) -> bool {
        self.axes == Self::ALL
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopAction {
    Disabled,
    LeftClick,
    RightClick,
    Enter,
    Escape,
    Overview,
    PageUp,
    PageDown,
}
impl DesktopAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::LeftClick => "left-click",
            Self::RightClick => "right-click",
            Self::Enter => "enter",
            Self::Escape => "escape",
            Self::Overview => "overview",
            Self::PageUp => "page-up",
            Self::PageDown => "page-down",
        }
    }
    pub fn parse(v: &str) -> Option<Self> {
        match v {
            "disabled" => Some(Self::Disabled),
            "left-click" => Some(Self::LeftClick),
            "right-click" => Some(Self::RightClick),
            "enter" => Some(Self::Enter),
            "escape" => Some(Self::Escape),
            "overview" => Some(Self::Overview),
            "page-up" => Some(Self::PageUp),
            "page-down" => Some(Self::PageDown),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopBindings {
    pub a: DesktopAction,
    pub b: DesktopAction,
    pub plus: DesktopAction,
    pub minus: DesktopAction,
    pub home: DesktopAction,
    pub one: DesktopAction,
    pub two: DesktopAction,
}
impl Default for DesktopBindings {
    fn default() -> Self {
        Self {
            a: DesktopAction::LeftClick,
            b: DesktopAction::RightClick,
            plus: DesktopAction::Enter,
            minus: DesktopAction::Escape,
            home: DesktopAction::Overview,
            one: DesktopAction::PageDown,
            two: DesktopAction::PageUp,
        }
    }
}
impl DesktopBindings {
    fn set(&mut self, name: &str, value: DesktopAction) -> bool {
        match name {
            "a" => self.a = value,
            "b" => self.b = value,
            "plus" => self.plus = value,
            "minus" => self.minus = value,
            "home" => self.home = value,
            "one" => self.one = value,
            "two" => self.two = value,
            _ => return false,
        };
        true
    }
    fn iter(&self) -> [(&'static str, DesktopAction); 7] {
        [
            ("a", self.a),
            ("b", self.b),
            ("plus", self.plus),
            ("minus", self.minus),
            ("home", self.home),
            ("one", self.one),
            ("two", self.two),
        ]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceRuleKind {
    Syspath,
    Devtype,
}
impl DeviceRuleKind {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Syspath => "device",
            Self::Devtype => "device-type",
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceRule {
    pub kind: DeviceRuleKind,
    pub match_text: String,
    pub profile: Profile,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub backend: Backend,
    pub profile: Profile,
    pub pointer_speed: i32,
    pub ir_speed: i32,
    pub ir_deadzone: i32,
    pub ir_smoothing: i32,
    pub ir_tracking: IrTracking,
    pub ir_aim_mapping: IrAimMapping,
    pub ir_screen: Option<IrRectangle>,
    pub aim_mode: AimMode,
    pub aim_source: AimSource,
    pub aim_activation: AimActivation,
    pub aim_sensitivity: i32,
    pub aim_deadzone: i32,
    pub aim_smoothing: i32,
    pub aim_invert_x: bool,
    pub aim_invert_y: bool,
    pub aim_accel_zero: Option<SensorCalibration>,
    pub aim_motion_plus_bias: Option<SensorCalibration>,
    pub aim_calibration_duration: i32,
    pub desktop_bindings: DesktopBindings,
    pub device_rules: Vec<DeviceRule>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            backend: Backend::Uinput,
            profile: Profile::GAMEPAD,
            pointer_speed: 16,
            ir_speed: 8,
            ir_deadzone: 0,
            ir_smoothing: 0,
            ir_tracking: IrTracking::Dual,
            ir_aim_mapping: IrAimMapping::Relative,
            ir_screen: None,
            aim_mode: AimMode::Off,
            aim_source: AimSource::Auto,
            aim_activation: AimActivation::B,
            aim_sensitivity: 16,
            aim_deadzone: 4,
            aim_smoothing: 25,
            aim_invert_x: false,
            aim_invert_y: false,
            aim_accel_zero: None,
            aim_motion_plus_bias: None,
            aim_calibration_duration: 8,
            desktop_bindings: DesktopBindings::default(),
            device_rules: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct ConfigError {
    pub path: PathBuf,
    pub line: Option<usize>,
    pub message: String,
    pub source: Option<io::Error>,
}
impl ConfigError {
    fn line(path: &str, line: usize, message: String) -> Self {
        Self {
            path: PathBuf::from(path),
            line: Some(line),
            message,
            source: None,
        }
    }
    pub(crate) fn io(path: &Path, source: io::Error) -> Self {
        Self {
            path: path.to_path_buf(),
            line: None,
            message: source.to_string(),
            source: Some(source),
        }
    }
}
impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(line) = self.line {
            write!(
                f,
                "wiilandd: {}:{}: {}",
                self.path.display(),
                line,
                self.message
            )
        } else {
            write!(
                f,
                "wiilandd: cannot open {}: {}",
                self.path.display(),
                self.message
            )
        }
    }
}
impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|e| e as _)
    }
}

impl Config {
    /// Parse one complete configuration without consulting the filesystem or environment.
    pub fn parse_bytes(path: impl AsRef<Path>, bytes: &[u8]) -> Result<Self, ConfigError> {
        let mut config = Self::default();
        config.apply_bytes(path.as_ref(), bytes)?;
        config.validate()?;
        Ok(config)
    }

    /// Apply a layer; validation is deferred until all layers have been applied.
    pub fn apply_bytes(&mut self, path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
        let mut start = 0;
        let mut no = 1;
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'\n' {
                let raw = &bytes[start..=i];
                if raw.len() > MAX_LINE_BYTES - 1 {
                    return Err(ConfigError::line(
                        &path.to_string_lossy(),
                        no,
                        "line too long".into(),
                    ));
                }
                let text = std::str::from_utf8(raw).map_err(|_| {
                    ConfigError::line(&path.to_string_lossy(), no, "invalid UTF-8".into())
                })?;
                self.apply_line(path, no, text)?;
                start = i + 1;
                no += 1;
            }
        }
        if start < bytes.len() {
            let raw = &bytes[start..];
            if raw.len() > MAX_LINE_BYTES - 1 {
                return Err(ConfigError::line(
                    &path.to_string_lossy(),
                    no,
                    "line too long".into(),
                ));
            }
            let text = std::str::from_utf8(raw).map_err(|_| {
                ConfigError::line(&path.to_string_lossy(), no, "invalid UTF-8".into())
            })?;
            self.apply_line(path, no, text)?;
        }
        Ok(())
    }
    pub fn profile_for_device(&self, syspath: Option<&str>, devtype: Option<&str>) -> Profile {
        let mut selected = self.profile;
        for rule in &self.device_rules {
            let matched = match rule.kind {
                DeviceRuleKind::Syspath => {
                    syspath.is_some_and(|path| path.contains(&rule.match_text))
                }
                DeviceRuleKind::Devtype => {
                    devtype.is_some_and(|kind| kind.contains(&rule.match_text))
                }
            };
            if matched {
                selected = rule.profile;
            }
        }
        selected
    }
    pub fn profile_for_syspath(&self, syspath: &str) -> Profile {
        self.profile_for_device(Some(syspath), None)
    }
    pub fn apply_line(
        &mut self,
        path: impl AsRef<Path>,
        line_no: usize,
        line: &str,
    ) -> Result<(), ConfigError> {
        let path = path.as_ref().to_string_lossy();
        let bytes = line.as_bytes();
        if bytes.len() > MAX_LINE_BYTES - 1 {
            return Err(ConfigError::line(&path, line_no, "line too long".into()));
        }
        let content = line.split('#').next().unwrap_or("");
        let content = content.trim_matches(|c| c == ' ' || c == '\t' || c == '\r' || c == '\n');
        if content.is_empty() {
            return Ok(());
        }
        let Some(eq) = content.find('=') else {
            return Err(ConfigError::line(
                &path,
                line_no,
                "expected key=value".into(),
            ));
        };
        let key = content[..eq].trim_matches(|c| c == ' ' || c == '\t' || c == '\r' || c == '\n');
        let value =
            content[eq + 1..].trim_matches(|c| c == ' ' || c == '\t' || c == '\r' || c == '\n');
        self.set_key(&path, line_no, key, value)
    }
    fn invalid(path: &str, line: usize, key: &str) -> ConfigError {
        ConfigError::line(path, line, format!("invalid value for '{}'", key))
    }
    fn set_key(
        &mut self,
        path: &str,
        line: usize,
        key: &str,
        value: &str,
    ) -> Result<(), ConfigError> {
        macro_rules! int {
            ($field:ident,$min:expr,$max:expr) => {{
                let v =
                    parse_int(value, $min, $max).ok_or_else(|| Self::invalid(path, line, key))?;
                self.$field = v;
                Ok(())
            }};
        }
        macro_rules! choice {
            ($field:ident,$ty:ty) => {{
                let v = <$ty>::parse(value).ok_or_else(|| Self::invalid(path, line, key))?;
                self.$field = v;
                Ok(())
            }};
        }
        match key {
            "backend" => {
                if value == "uinput" {
                    self.backend = Backend::Uinput;
                    Ok(())
                } else {
                    Err(Self::invalid(path, line, key))
                }
            }
            "profile" => {
                self.profile =
                    Profile::parse(value).ok_or_else(|| Self::invalid(path, line, key))?;
                Ok(())
            }
            "pointer-speed" => int!(pointer_speed, 1, 127),
            "ir-speed" => int!(ir_speed, 1, 127),
            "ir-deadzone" => int!(ir_deadzone, 0, 127),
            "ir-smoothing" => int!(ir_smoothing, 0, 95),
            "ir-tracking" => choice!(ir_tracking, IrTracking),
            "ir-aim-mapping" => choice!(ir_aim_mapping, IrAimMapping),
            "ir-screen-left" => self.set_screen(path, line, key, value, 0),
            "ir-screen-right" => self.set_screen(path, line, key, value, 1),
            "ir-screen-top" => self.set_screen(path, line, key, value, 2),
            "ir-screen-bottom" => self.set_screen(path, line, key, value, 3),
            "aim-mode" => choice!(aim_mode, AimMode),
            "aim-source" => choice!(aim_source, AimSource),
            "aim-activation" => choice!(aim_activation, AimActivation),
            "aim-sensitivity" => int!(aim_sensitivity, 1, 127),
            "aim-deadzone" => int!(aim_deadzone, 0, 32767),
            "aim-smoothing" => int!(aim_smoothing, 0, 95),
            "aim-invert-x" => self.set_bool(path, line, key, value, true),
            "aim-invert-y" => self.set_bool(path, line, key, value, false),
            "aim-accel-zero-x" => self.set_cal(path, line, key, value, true, 0),
            "aim-accel-zero-y" => self.set_cal(path, line, key, value, true, 1),
            "aim-accel-zero-z" => self.set_cal(path, line, key, value, true, 2),
            "aim-motion-plus-bias-x" => self.set_cal(path, line, key, value, false, 0),
            "aim-motion-plus-bias-y" => self.set_cal(path, line, key, value, false, 1),
            "aim-motion-plus-bias-z" => self.set_cal(path, line, key, value, false, 2),
            "aim-calibration-duration" => int!(aim_calibration_duration, 1, 30),
            _ if key.strip_prefix("desktop.").is_some() => {
                let name = &key[8..];
                let action =
                    DesktopAction::parse(value).ok_or_else(|| Self::invalid(path, line, key))?;
                if self.desktop_bindings.set(name, action) {
                    Ok(())
                } else {
                    Err(Self::invalid(path, line, key))
                }
            }
            _ if key.starts_with("device.") && key.ends_with(".profile") => {
                self.set_rule(path, line, key, value, DeviceRuleKind::Syspath)
            }
            _ if key.starts_with("device-type.") && key.ends_with(".profile") => {
                self.set_rule(path, line, key, value, DeviceRuleKind::Devtype)
            }
            _ => Err(ConfigError::line(
                path,
                line,
                format!("unknown key '{}'", key),
            )),
        }
    }
    fn set_screen(
        &mut self,
        path: &str,
        line: usize,
        key: &str,
        value: &str,
        axis: usize,
    ) -> Result<(), ConfigError> {
        let v = parse_int(value, 0, 32767).ok_or_else(|| Self::invalid(path, line, key))?;
        let r = self.ir_screen.get_or_insert(IrRectangle {
            left: 0,
            right: 1023,
            top: 0,
            bottom: 767,
        });
        match axis {
            0 => r.left = v,
            1 => r.right = v,
            2 => r.top = v,
            _ => r.bottom = v,
        };
        Ok(())
    }
    fn set_bool(
        &mut self,
        path: &str,
        line: usize,
        key: &str,
        value: &str,
        x: bool,
    ) -> Result<(), ConfigError> {
        let v = match value {
            "yes" | "true" | "1" => true,
            "no" | "false" | "0" => false,
            _ => return Err(Self::invalid(path, line, key)),
        };
        if x {
            self.aim_invert_x = v
        } else {
            self.aim_invert_y = v
        };
        Ok(())
    }
    fn set_cal(
        &mut self,
        path: &str,
        line: usize,
        key: &str,
        value: &str,
        accel: bool,
        axis: usize,
    ) -> Result<(), ConfigError> {
        let v = parse_int(value, -32768, 32767).ok_or_else(|| Self::invalid(path, line, key))?;
        let cal = (if accel {
            &mut self.aim_accel_zero
        } else {
            &mut self.aim_motion_plus_bias
        })
        .get_or_insert(SensorCalibration {
            x: 0,
            y: 0,
            z: 0,
            axes: 0,
        });
        match axis {
            0 => {
                cal.x = v;
                cal.axes |= SensorCalibration::X
            }
            1 => {
                cal.y = v;
                cal.axes |= SensorCalibration::Y
            }
            _ => {
                cal.z = v;
                cal.axes |= SensorCalibration::Z
            }
        };
        Ok(())
    }
    fn set_rule(
        &mut self,
        path: &str,
        line: usize,
        key: &str,
        value: &str,
        kind: DeviceRuleKind,
    ) -> Result<(), ConfigError> {
        let profile = Profile::parse(value).ok_or_else(|| Self::invalid(path, line, key))?;
        let prefix = kind.prefix();
        let middle = &key[prefix.len() + 1..key.len() - 8];
        if middle.is_empty() {
            return Err(Self::invalid(path, line, key));
        }
        if let Some(i) = self
            .device_rules
            .iter()
            .position(|r| r.kind == kind && r.match_text == middle)
        {
            let mut rule = self.device_rules.remove(i);
            rule.profile = profile;
            self.device_rules.push(rule);
        } else {
            if self.device_rules.len() >= MAX_DEVICE_RULES {
                return Err(Self::invalid(path, line, key));
            }
            self.device_rules.push(DeviceRule {
                kind,
                match_text: middle.to_owned(),
                profile,
            });
        }
        Ok(())
    }
    pub fn validate(&self) -> Result<(), ConfigError> {
        let bad = |msg: &str| ConfigError {
            path: PathBuf::from("config"),
            line: None,
            message: msg.into(),
            source: None,
        };
        if !self.profile.is_valid() {
            return Err(bad("invalid profile"));
        }
        if !(1..=127).contains(&self.pointer_speed)
            || !(1..=127).contains(&self.ir_speed)
            || !(0..=127).contains(&self.ir_deadzone)
            || !(0..=95).contains(&self.ir_smoothing)
            || !(1..=127).contains(&self.aim_sensitivity)
            || !(0..=32767).contains(&self.aim_deadzone)
            || !(0..=95).contains(&self.aim_smoothing)
            || !(1..=30).contains(&self.aim_calibration_duration)
        {
            return Err(bad("configuration value out of range"));
        }
        if let Some(r) = self.ir_screen
            && (r.right <= r.left || r.bottom <= r.top)
        {
            return Err(bad(
                "IR screen calibration requires right > left and bottom > top",
            ));
        }
        for (name, c) in [
            ("accelerometer", self.aim_accel_zero),
            ("MotionPlus", self.aim_motion_plus_bias),
        ] {
            if let Some(c) = c {
                if c.axes & !SensorCalibration::ALL != 0 {
                    return Err(bad("invalid sensor calibration axes"));
                }
                if c.axes != 0 && !c.complete() {
                    return Err(bad(&format!(
                        "{} calibration requires complete x, y, and z values",
                        name
                    )));
                }
            }
        }
        if self.device_rules.len() > MAX_DEVICE_RULES
            || self
                .device_rules
                .iter()
                .any(|r| r.match_text.is_empty() || !r.profile.is_valid())
        {
            return Err(bad("invalid device rule"));
        }
        Ok(())
    }
    pub fn dump(&self) -> String {
        let mut out = String::new();
        macro_rules! line {($($arg:tt)*)=>{{out.push_str(&format!($($arg)*));out.push('\n')}}}
        line!("backend={}", self.backend.as_str());
        line!("profile={}", self.profile.as_str().unwrap_or("unknown"));
        line!("pointer-speed={}", self.pointer_speed);
        line!("ir-speed={}", self.ir_speed);
        line!("ir-deadzone={}", self.ir_deadzone);
        line!("ir-smoothing={}", self.ir_smoothing);
        line!("ir-tracking={}", self.ir_tracking.as_str());
        line!("ir-aim-mapping={}", self.ir_aim_mapping.as_str());
        if let Some(r) = self.ir_screen {
            line!("ir-screen-left={}", r.left);
            line!("ir-screen-right={}", r.right);
            line!("ir-screen-top={}", r.top);
            line!("ir-screen-bottom={}", r.bottom);
        }
        line!("aim-mode={}", self.aim_mode.as_str());
        line!("aim-source={}", self.aim_source.as_str());
        line!("aim-activation={}", self.aim_activation.as_str());
        line!("aim-sensitivity={}", self.aim_sensitivity);
        line!("aim-deadzone={}", self.aim_deadzone);
        line!("aim-smoothing={}", self.aim_smoothing);
        line!(
            "aim-invert-x={}",
            if self.aim_invert_x { "yes" } else { "no" }
        );
        line!(
            "aim-invert-y={}",
            if self.aim_invert_y { "yes" } else { "no" }
        );
        if let Some(c) = self.aim_accel_zero
            && c.complete()
        {
            line!("aim-accel-zero-x={}", c.x);
            line!("aim-accel-zero-y={}", c.y);
            line!("aim-accel-zero-z={}", c.z);
        }
        if let Some(c) = self.aim_motion_plus_bias
            && c.complete()
        {
            line!("aim-motion-plus-bias-x={}", c.x);
            line!("aim-motion-plus-bias-y={}", c.y);
            line!("aim-motion-plus-bias-z={}", c.z);
        }
        line!("aim-calibration-duration={}", self.aim_calibration_duration);
        for (name, a) in self.desktop_bindings.iter() {
            line!("desktop.{}={}", name, a.as_str());
        }
        for r in &self.device_rules {
            line!(
                "{}.{}.profile={}",
                r.kind.prefix(),
                r.match_text,
                r.profile.as_str().unwrap_or("unknown")
            );
        }
        out
    }
}

fn parse_int(v: &str, min: i32, max: i32) -> Option<i32> {
    let n = v.parse::<i64>().ok()?;
    if n < i64::from(min) || n > i64::from(max) {
        None
    } else {
        Some(n as i32)
    }
}

/// A validated snapshot. Mutable drafts remain ordinary `Config` values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedConfig(Config);

impl TryFrom<Config> for ValidatedConfig {
    type Error = ConfigError;
    fn try_from(config: Config) -> Result<Self, Self::Error> {
        config.validate()?;
        Ok(Self(config))
    }
}
impl ValidatedConfig {
    pub fn get(&self) -> &Config {
        &self.0
    }
}
