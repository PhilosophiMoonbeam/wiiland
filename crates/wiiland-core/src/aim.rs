//! Sticky motion-aim source, activation, calibration, and smoothing state.
//!
//! Persistent option types are owned by `config`; this module owns only the
//! transient runtime state and integer output calculations.

use crate::calibration::sensor_calibration_ready;
use crate::config::{
    AimActivation, AimMode, AimSource, Config, IrAimMapping, IrRectangle, SensorCalibration,
};
use crate::input::Button;
use crate::pointer::IrPoint;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AimVector {
    pub x: i32,
    pub y: i32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AimResult {
    pub output: Option<AimVector>,
    pub reset: bool,
}
impl AimResult {
    const NONE: Self = Self {
        output: None,
        reset: false,
    };
    fn reset() -> Self {
        Self {
            output: Some(AimVector { x: 0, y: 0 }),
            reset: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AimConfig {
    pub output: AimMode,
    pub source: AimSource,
    pub activation: AimActivation,
    pub sensitivity: i32,
    pub deadzone: i32,
    pub smoothing: i32,
    pub invert_x: bool,
    pub invert_y: bool,
    pub ir_mapping: IrAimMapping,
    pub ir_screen: Option<IrRectangle>,
    pub accel_calibration: Option<SensorCalibration>,
    pub motion_plus_calibration: Option<SensorCalibration>,
}

impl AimConfig {
    pub fn from_config(config: &Config) -> Self {
        Self {
            output: config.aim_mode,
            source: config.aim_source,
            activation: config.aim_activation,
            sensitivity: config.aim_sensitivity,
            deadzone: config.aim_deadzone,
            smoothing: config.aim_smoothing,
            invert_x: config.aim_invert_x,
            invert_y: config.aim_invert_y,
            ir_mapping: config.ir_aim_mapping,
            ir_screen: config.ir_screen,
            accel_calibration: config.aim_accel_zero,
            motion_plus_calibration: config.aim_motion_plus_bias,
        }
    }
}
impl Default for AimConfig {
    fn default() -> Self {
        Self {
            output: AimMode::Off,
            source: AimSource::Auto,
            activation: AimActivation::B,
            sensitivity: 16,
            deadzone: 4,
            smoothing: 25,
            invert_x: false,
            invert_y: false,
            ir_mapping: IrAimMapping::Relative,
            ir_screen: None,
            accel_calibration: None,
            motion_plus_calibration: None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AimState {
    pub config: AimConfig,
    pub held: bool,
    pub active_source: Option<AimSource>,
    pub ir_active: bool,
    pub ir_x: i32,
    pub ir_y: i32,
    pub accel_zeroed: bool,
    pub accel_zero_x: i32,
    pub accel_zero_y: i32,
    pub accel_zero_z: i32,
    pub last_x: i32,
    pub last_y: i32,
}
impl AimState {
    pub const fn new(config: AimConfig) -> Self {
        Self {
            config,
            held: false,
            active_source: None,
            ir_active: false,
            ir_x: 0,
            ir_y: 0,
            accel_zeroed: false,
            accel_zero_x: 0,
            accel_zero_y: 0,
            accel_zero_z: 0,
            last_x: 0,
            last_y: 0,
        }
    }
    pub fn enabled(&self) -> bool {
        self.config.output != AimMode::Off
    }
    pub fn is_active(&self) -> bool {
        self.enabled() && (self.config.activation == AimActivation::Always || self.held)
    }
    /// Forget tracking baselines while retaining the current activation key.
    pub fn reset(&mut self) -> AimResult {
        self.active_source = None;
        self.ir_active = false;
        self.accel_zeroed = false;
        self.last_x = 0;
        self.last_y = 0;
        AimResult::reset()
    }
    /// Start a new input session after outputs or their input interfaces disappear.
    pub fn reset_session(&mut self) -> AimResult {
        self.held = false;
        self.reset()
    }
    /// Update the configured activation key. Releasing it resets source,
    /// baselines, and smoothing; holding remains sticky until release.
    pub fn activation_key(&mut self, key: u32, pressed: bool) -> AimResult {
        if !self.enabled() {
            return AimResult::NONE;
        }
        if self.config.activation == AimActivation::Always {
            self.held = true;
            return AimResult::NONE;
        }
        let expected = match self.config.activation {
            AimActivation::B => Button::B.code(),
            AimActivation::Z => Button::Z.code(),
            AimActivation::C => Button::C.code(),
            AimActivation::Always => u32::MAX,
        };
        if key != expected {
            return AimResult::NONE;
        }
        let was_held = self.held;
        self.held = pressed;
        if was_held && !pressed {
            self.reset()
        } else {
            AimResult::NONE
        }
    }
    fn accepts(&self, source: AimSource) -> bool {
        if self.config.source != AimSource::Auto {
            return self.config.source == source;
        }
        if source == AimSource::Ir {
            return true;
        }
        if self.active_source == Some(AimSource::Ir) {
            return false;
        }
        if source == AimSource::MotionPlus {
            return true;
        }
        self.active_source != Some(AimSource::MotionPlus) || source != AimSource::Accelerometer
    }
    fn scale(&self, value: i32) -> i32 {
        let magnitude = i64::from(value).abs();
        if magnitude <= i64::from(self.config.deadzone) {
            return 0;
        }
        let scaled = ((magnitude - i64::from(self.config.deadzone))
            * i64::from(self.config.sensitivity))
            / 16;
        let scaled = scaled.min(32_767) as i32;
        if value < 0 { -scaled } else { scaled }
    }
    fn smooth(&mut self, mut vector: AimVector) -> AimVector {
        if self.config.smoothing == 0 {
            self.last_x = vector.x;
            self.last_y = vector.y;
            return vector;
        }
        vector.x = (i64::from(self.last_x) * i64::from(self.config.smoothing)
            + i64::from(vector.x) * i64::from(100 - self.config.smoothing))
            as i32
            / 100;
        vector.y = (i64::from(self.last_y) * i64::from(self.config.smoothing)
            + i64::from(vector.y) * i64::from(100 - self.config.smoothing))
            as i32
            / 100;
        self.last_x = vector.x;
        self.last_y = vector.y;
        vector
    }
    fn emit(&mut self, mut vector: AimVector) -> AimResult {
        if !self.is_active() {
            return self.reset();
        }
        if self.config.invert_x {
            vector.x = -vector.x;
        }
        if self.config.invert_y {
            vector.y = -vector.y;
        }
        AimResult {
            output: Some(self.smooth(vector)),
            reset: false,
        }
    }
    pub fn process_ir(&mut self, point: Option<IrPoint>) -> AimResult {
        let Some(point) = point.filter(|point| point.valid) else {
            if self.active_source == Some(AimSource::Ir) {
                return self.reset();
            }
            self.ir_active = false;
            return AimResult::NONE;
        };
        if !self.accepts(AimSource::Ir) {
            return AimResult::NONE;
        }
        let screen = self.config.ir_screen;
        let vector = if let Some(s) = screen
            .filter(|s| s.right > s.left && s.bottom > s.top)
            .filter(|_| self.config.ir_mapping == IrAimMapping::Absolute)
        {
            AimVector {
                x: self.scale(scale_ir_absolute_axis(point.x, s.left, s.right)),
                y: self.scale(scale_ir_absolute_axis(point.y, s.top, s.bottom)),
            }
        } else if self.ir_active {
            AimVector {
                x: self.scale(point.x.wrapping_sub(self.ir_x)),
                y: self.scale(point.y.wrapping_sub(self.ir_y)),
            }
        } else {
            AimVector::default()
        };
        self.ir_active = true;
        self.ir_x = point.x;
        self.ir_y = point.y;
        self.active_source = Some(AimSource::Ir);
        self.emit(vector)
    }
    pub fn process_motion_plus(&mut self, sample: [i32; 3]) -> AimResult {
        if !self.accepts(AimSource::MotionPlus) {
            return AimResult::NONE;
        }
        let mut x = sample[0];
        let mut y = sample[1];
        if let Some(c) = self
            .config
            .motion_plus_calibration
            .filter(sensor_calibration_ready)
        {
            x -= c.x;
            y -= c.y;
        }
        self.active_source = Some(AimSource::MotionPlus);
        self.emit(AimVector {
            x: self.scale(x),
            y: self.scale(y),
        })
    }
    pub fn process_accelerometer(&mut self, sample: [i32; 3]) -> AimResult {
        if !self.accepts(AimSource::Accelerometer) {
            return AimResult::NONE;
        }
        if let Some(c) = self
            .config
            .accel_calibration
            .filter(sensor_calibration_ready)
        {
            self.accel_zero_x = c.x;
            self.accel_zero_y = c.y;
            self.accel_zero_z = c.z;
            self.accel_zeroed = true;
        } else if !self.accel_zeroed {
            self.accel_zero_x = sample[0];
            self.accel_zero_y = sample[1];
            self.accel_zero_z = sample[2];
            self.accel_zeroed = true;
            self.active_source = Some(AimSource::Accelerometer);
            return AimResult::NONE;
        }
        self.active_source = Some(AimSource::Accelerometer);
        self.emit(AimVector {
            x: self.scale(sample[0] - self.accel_zero_x),
            y: self.scale(sample[1] - self.accel_zero_y),
        })
    }
}

pub fn scale_ir_absolute_axis(value: i32, min: i32, max: i32) -> i32 {
    let center = (i64::from(min) + i64::from(max)) / 2;
    let half_range = (i64::from(max) - i64::from(min)) / 2;
    if half_range <= 0 {
        return 0;
    }
    ((i64::from(value) - center) * 32_767 / half_range).clamp(-32_767, 32_767) as i32
}
