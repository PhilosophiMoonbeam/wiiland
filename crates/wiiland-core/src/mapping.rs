//! Integer-only Wii input to virtual evdev mappings.
//!
//! Event values are processed with signed 64-bit intermediates to preserve the
//! original truncation and asymmetric extrema rules without floating point.

use crate::input::Button;

pub const VIRTUAL_AXIS_MIN: i32 = -32_768;
pub const VIRTUAL_AXIS_MAX: i32 = 32_767;
pub const VIRTUAL_TRIGGER_MAX: i32 = 1_023;
pub const WIIMOTE_ACCEL_AXIS_EXTENT: i32 = 500;
pub const NUNCHUK_STICK_AXIS_EXTENT: i32 = 120;
pub const CLASSIC_STICK_AXIS_EXTENT: i32 = 30;
pub const CLASSIC_TRIGGER_MAX: i32 = 62;
pub const PRO_STICK_AXIS_EXTENT: i32 = 1_024;
pub const RHYTHM_STICK_NEGATIVE_EXTENT: i32 = 32;
pub const RHYTHM_STICK_POSITIVE_EXTENT: i32 = 31;
pub const DRUM_PRESSURE_MAX: i32 = 7;
pub const GUITAR_WHAMMY_NEGATIVE_EXTENT: i32 = 16;
pub const GUITAR_WHAMMY_POSITIVE_EXTENT: i32 = 15;
pub const GUITAR_FRET_MAX: i32 = 31;
pub const MOTION_PLUS_AXIS_EXTENT: i32 = 16_000;

// Linux input-event-codes values. Core remains display and libc independent.
pub const BTN_SOUTH: u16 = 0x130;
pub const BTN_EAST: u16 = 0x131;
pub const BTN_NORTH: u16 = 0x133;
pub const BTN_WEST: u16 = 0x134;
pub const BTN_TL: u16 = 0x136;
pub const BTN_TR: u16 = 0x137;
pub const BTN_TL2: u16 = 0x138;
pub const BTN_TR2: u16 = 0x139;
pub const BTN_SELECT: u16 = 0x13a;
pub const BTN_START: u16 = 0x13b;
pub const BTN_MODE: u16 = 0x13c;
pub const BTN_THUMBL: u16 = 0x13d;
pub const BTN_THUMBR: u16 = 0x13e;
pub const BTN_DPAD_LEFT: u16 = 0x222;
pub const BTN_DPAD_RIGHT: u16 = 0x223;
pub const BTN_DPAD_UP: u16 = 0x220;
pub const BTN_DPAD_DOWN: u16 = 0x221;
pub const BTN_1: u16 = 0x101;
pub const BTN_2: u16 = 0x102;
pub const BTN_C: u16 = 0x132;
pub const BTN_Z: u16 = 0x135;
pub const BTN_STRUM_BAR_UP: u16 = 0x229;
pub const BTN_STRUM_BAR_DOWN: u16 = 0x22a;
pub const BTN_FRET_FAR_UP: u16 = 0x224;
pub const BTN_FRET_UP: u16 = 0x225;
pub const BTN_FRET_MID: u16 = 0x226;
pub const BTN_FRET_LOW: u16 = 0x227;
pub const BTN_FRET_FAR_LOW: u16 = 0x228;
pub const ABS_X: u16 = 0;
pub const ABS_Y: u16 = 1;
pub const ABS_Z: u16 = 2;
pub const ABS_RX: u16 = 3;
pub const ABS_RY: u16 = 4;
pub const ABS_RZ: u16 = 5;
pub const ABS_THROTTLE: u16 = 6;
pub const ABS_RUDDER: u16 = 7;
pub const ABS_WHEEL: u16 = 8;
pub const ABS_GAS: u16 = 9;
pub const ABS_BRAKE: u16 = 10;
pub const ABS_HAT0X: u16 = 16;
pub const ABS_HAT1X: u16 = 18;
pub const ABS_HAT1Y: u16 = 19;
pub const ABS_HAT2X: u16 = 20;
pub const ABS_HAT3X: u16 = 22;
pub const ABS_HAT3Y: u16 = 23;
pub const ABS_MISC: u16 = 40;
pub const ABS_PRESSURE: u16 = 24;
pub const ABS_DISTANCE: u16 = 25;
pub const ABS_TILT_X: u16 = 26;
pub const ABS_TILT_Y: u16 = 27;

pub const CORE_KEYS: [u16; 28] = [
    BTN_DPAD_LEFT,
    BTN_DPAD_RIGHT,
    BTN_DPAD_UP,
    BTN_DPAD_DOWN,
    BTN_SOUTH,
    BTN_EAST,
    BTN_NORTH,
    BTN_WEST,
    BTN_START,
    BTN_SELECT,
    BTN_MODE,
    BTN_1,
    BTN_2,
    BTN_TL,
    BTN_TR,
    BTN_TL2,
    BTN_TR2,
    BTN_THUMBL,
    BTN_THUMBR,
    BTN_C,
    BTN_Z,
    BTN_STRUM_BAR_UP,
    BTN_STRUM_BAR_DOWN,
    BTN_FRET_FAR_UP,
    BTN_FRET_UP,
    BTN_FRET_MID,
    BTN_FRET_LOW,
    BTN_FRET_FAR_LOW,
];
pub const CORE_AXES: [u16; 22] = [
    ABS_X,
    ABS_Y,
    ABS_RX,
    ABS_RY,
    ABS_Z,
    ABS_RZ,
    ABS_HAT3X,
    ABS_HAT3Y,
    ABS_MISC,
    ABS_PRESSURE,
    ABS_DISTANCE,
    ABS_TILT_X,
    ABS_TILT_Y,
    ABS_THROTTLE,
    ABS_RUDDER,
    ABS_WHEEL,
    ABS_GAS,
    ABS_BRAKE,
    ABS_HAT0X,
    ABS_HAT1X,
    ABS_HAT1Y,
    ABS_HAT2X,
];
pub const DESKTOP_KEYS: [u16; 7] = [0x110, 0x111, 28, 1, 125, 104, 109];
pub const DESKTOP_RELS: [u16; 2] = [0, 1];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AxisInfo {
    pub minimum: i32,
    pub maximum: i32,
    pub flat: i32,
    pub fuzz: i32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtualCapabilities {
    pub keys: &'static [u16],
    pub axes: &'static [u16],
    pub rels: &'static [u16],
}
pub const CONTROLLER_CAPABILITIES: VirtualCapabilities = VirtualCapabilities {
    keys: &CORE_KEYS,
    axes: &CORE_AXES,
    rels: &[],
};
pub const DESKTOP_CAPABILITIES: VirtualCapabilities = VirtualCapabilities {
    keys: &DESKTOP_KEYS,
    axes: &[],
    rels: &DESKTOP_RELS,
};

pub fn clamp_axis_value(value: i32) -> i32 {
    value.clamp(VIRTUAL_AXIS_MIN, VIRTUAL_AXIS_MAX)
}
pub fn scale_signed_axis(value: i32, negative_extent: i32, positive_extent: i32) -> i32 {
    if negative_extent <= 0 || positive_extent <= 0 {
        return 0;
    }
    if value <= -negative_extent {
        return VIRTUAL_AXIS_MIN;
    }
    if value >= positive_extent {
        return VIRTUAL_AXIS_MAX;
    }
    if value < 0 {
        (i64::from(value) * i64::from(-VIRTUAL_AXIS_MIN) / i64::from(negative_extent)) as i32
    } else {
        (i64::from(value) * i64::from(VIRTUAL_AXIS_MAX) / i64::from(positive_extent)) as i32
    }
}
pub fn scale_unsigned_axis(value: i32, source_max: i32, target_max: i32) -> i32 {
    if source_max <= 0 || target_max < 0 || value <= 0 {
        return 0;
    }
    if value >= source_max {
        return target_max;
    }
    (i64::from(value) * i64::from(target_max) / i64::from(source_max)) as i32
}

/// Compatibility adapter for stable logical button numbers.
pub fn map_key(code: u32) -> Option<u16> {
    Button::from_code(code).map(map_button)
}

pub fn map_button(button: Button) -> u16 {
    match button {
        Button::Left => BTN_DPAD_LEFT,
        Button::Right => BTN_DPAD_RIGHT,
        Button::Up => BTN_DPAD_UP,
        Button::Down => BTN_DPAD_DOWN,
        Button::A => BTN_SOUTH,
        Button::B => BTN_EAST,
        Button::Plus => BTN_START,
        Button::Minus => BTN_SELECT,
        Button::Home => BTN_MODE,
        Button::One => BTN_1,
        Button::Two => BTN_2,
        Button::X => BTN_NORTH,
        Button::Y => BTN_WEST,
        Button::ShoulderLeft => BTN_TL,
        Button::ShoulderRight => BTN_TR,
        Button::TriggerLeft => BTN_TL2,
        Button::TriggerRight => BTN_TR2,
        Button::ThumbLeft => BTN_THUMBL,
        Button::ThumbRight => BTN_THUMBR,
        Button::C => BTN_C,
        Button::Z => BTN_Z,
        Button::StrumBarUp => BTN_STRUM_BAR_UP,
        Button::StrumBarDown => BTN_STRUM_BAR_DOWN,
        Button::FretFarUp => BTN_FRET_FAR_UP,
        Button::FretUp => BTN_FRET_UP,
        Button::FretMid => BTN_FRET_MID,
        Button::FretLow => BTN_FRET_LOW,
        Button::FretFarLow => BTN_FRET_FAR_LOW,
    }
}
pub fn accel_abs_code(index: usize) -> Option<u16> {
    [ABS_THROTTLE, ABS_RUDDER, ABS_WHEEL].get(index).copied()
}
pub fn nunchuk_accel_abs_code(index: usize) -> Option<u16> {
    [ABS_HAT1X, ABS_HAT1Y, ABS_HAT2X].get(index).copied()
}
pub fn motion_plus_abs_code(index: usize) -> Option<u16> {
    [ABS_GAS, ABS_BRAKE, ABS_HAT0X].get(index).copied()
}
pub fn balance_abs_code(index: usize) -> Option<u16> {
    [ABS_PRESSURE, ABS_DISTANCE, ABS_TILT_X, ABS_TILT_Y]
        .get(index)
        .copied()
}
pub fn drums_abs_code(index: usize) -> Option<u16> {
    [
        ABS_X, ABS_RX, ABS_RY, ABS_Z, ABS_RZ, ABS_HAT3X, ABS_HAT3Y, ABS_MISC,
    ]
    .get(index)
    .copied()
}
pub fn scale_drums_pressure(index: usize, value: i32) -> i32 {
    let max = if matches!(index, 1 | 2 | 5) {
        VIRTUAL_AXIS_MAX
    } else {
        VIRTUAL_TRIGGER_MAX
    };
    scale_unsigned_axis(value, DRUM_PRESSURE_MAX, max)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Abs3 {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappedAxis {
    pub code: u16,
    pub value: i32,
}
pub fn map_xyz(value: Abs3, codes: [u16; 3], extent: i32) -> [MappedAxis; 3] {
    [
        MappedAxis {
            code: codes[0],
            value: scale_signed_axis(value.x, extent, extent),
        },
        MappedAxis {
            code: codes[1],
            value: scale_signed_axis(value.y, extent, extent),
        },
        MappedAxis {
            code: codes[2],
            value: scale_signed_axis(value.z, extent, extent),
        },
    ]
}

/// Axis metadata used when constructing the virtual controller. Unknown
/// codes are intentionally omitted rather than receiving a guessed range.
pub fn axis_info(code: u16) -> Option<AxisInfo> {
    if !CORE_AXES.contains(&code) {
        return None;
    }
    let mut info = AxisInfo {
        minimum: VIRTUAL_AXIS_MIN,
        maximum: VIRTUAL_AXIS_MAX,
        flat: 256,
        fuzz: 16,
    };
    if matches!(code, ABS_Z | ABS_RZ | ABS_HAT3Y | ABS_MISC) {
        info = AxisInfo {
            minimum: 0,
            maximum: VIRTUAL_TRIGGER_MAX,
            flat: 0,
            fuzz: 4,
        };
    } else if matches!(code, ABS_PRESSURE | ABS_DISTANCE | ABS_TILT_X | ABS_TILT_Y) {
        info = AxisInfo {
            minimum: 0,
            maximum: 65_535,
            flat: 0,
            fuzz: 4,
        };
    }
    Some(info)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MotionKind {
    Accel,
    Nunchuk,
    Classic,
    Pro,
    Guitar,
    Balance,
    MotionPlus,
    Drums,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappedMotion {
    pub axes: [MappedAxis; 9],
    pub count: usize,
}
impl Default for MappedMotion {
    fn default() -> Self {
        Self {
            axes: [MappedAxis { code: 0, value: 0 }; 9],
            count: 0,
        }
    }
}
impl MappedMotion {
    fn push(&mut self, code: u16, value: i32) {
        if self.count < self.axes.len() {
            self.axes[self.count] = MappedAxis { code, value };
            self.count += 1;
        }
    }
}

/// Translate one absolute event payload. `values` uses fixed logical slots;
/// only fields required by `kind` are read.
pub fn map_motion(kind: MotionKind, values: [Abs3; 8]) -> MappedMotion {
    let mut out = MappedMotion::default();
    match kind {
        MotionKind::Accel => {
            for axis in map_xyz(
                values[0],
                [ABS_THROTTLE, ABS_RUDDER, ABS_WHEEL],
                WIIMOTE_ACCEL_AXIS_EXTENT,
            ) {
                out.push(axis.code, axis.value);
            }
        }
        MotionKind::Nunchuk => {
            out.push(
                ABS_X,
                scale_signed_axis(
                    values[0].x,
                    NUNCHUK_STICK_AXIS_EXTENT,
                    NUNCHUK_STICK_AXIS_EXTENT,
                ),
            );
            out.push(
                ABS_Y,
                scale_signed_axis(
                    values[0].y,
                    NUNCHUK_STICK_AXIS_EXTENT,
                    NUNCHUK_STICK_AXIS_EXTENT,
                ),
            );
            for axis in map_xyz(
                values[1],
                [ABS_HAT1X, ABS_HAT1Y, ABS_HAT2X],
                WIIMOTE_ACCEL_AXIS_EXTENT,
            ) {
                out.push(axis.code, axis.value);
            }
        }
        MotionKind::Classic | MotionKind::Pro => {
            let extent = if kind == MotionKind::Classic {
                CLASSIC_STICK_AXIS_EXTENT
            } else {
                PRO_STICK_AXIS_EXTENT
            };
            out.push(ABS_X, scale_signed_axis(values[0].x, extent, extent));
            out.push(ABS_Y, scale_signed_axis(values[0].y, extent, extent));
            out.push(ABS_RX, scale_signed_axis(values[1].x, extent, extent));
            out.push(ABS_RY, scale_signed_axis(values[1].y, extent, extent));
            if kind == MotionKind::Classic {
                out.push(
                    ABS_Z,
                    scale_unsigned_axis(values[2].x, CLASSIC_TRIGGER_MAX, VIRTUAL_TRIGGER_MAX),
                );
                out.push(
                    ABS_RZ,
                    scale_unsigned_axis(values[2].y, CLASSIC_TRIGGER_MAX, VIRTUAL_TRIGGER_MAX),
                );
            }
        }
        MotionKind::Guitar => {
            out.push(
                ABS_X,
                scale_signed_axis(
                    values[0].x,
                    RHYTHM_STICK_NEGATIVE_EXTENT,
                    RHYTHM_STICK_POSITIVE_EXTENT,
                ),
            );
            out.push(
                ABS_Y,
                scale_signed_axis(
                    values[0].y,
                    RHYTHM_STICK_NEGATIVE_EXTENT,
                    RHYTHM_STICK_POSITIVE_EXTENT,
                ),
            );
            out.push(
                ABS_HAT3X,
                scale_signed_axis(
                    values[1].x,
                    GUITAR_WHAMMY_NEGATIVE_EXTENT,
                    GUITAR_WHAMMY_POSITIVE_EXTENT,
                ),
            );
            out.push(
                ABS_HAT3Y,
                scale_unsigned_axis(values[2].x, GUITAR_FRET_MAX, VIRTUAL_TRIGGER_MAX),
            );
        }
        MotionKind::Balance => {
            for (index, code) in [ABS_PRESSURE, ABS_DISTANCE, ABS_TILT_X, ABS_TILT_Y]
                .iter()
                .enumerate()
            {
                out.push(*code, values[index].x);
            }
        }
        MotionKind::MotionPlus => {
            for axis in map_xyz(
                values[0],
                [ABS_GAS, ABS_BRAKE, ABS_HAT0X],
                MOTION_PLUS_AXIS_EXTENT,
            ) {
                out.push(axis.code, axis.value);
            }
        }
        MotionKind::Drums => {
            out.push(
                ABS_X,
                scale_signed_axis(
                    values[0].x,
                    RHYTHM_STICK_NEGATIVE_EXTENT,
                    RHYTHM_STICK_POSITIVE_EXTENT,
                ),
            );
            out.push(
                ABS_Y,
                scale_signed_axis(
                    values[0].y,
                    RHYTHM_STICK_NEGATIVE_EXTENT,
                    RHYTHM_STICK_POSITIVE_EXTENT,
                ),
            );
            for (index, value) in values.iter().enumerate().skip(1) {
                out.push(
                    drums_abs_code(index).unwrap_or(0),
                    scale_drums_pressure(index, value.x),
                );
            }
        }
    }
    out
}
