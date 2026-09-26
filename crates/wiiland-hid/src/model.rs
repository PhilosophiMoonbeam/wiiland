//! Owned values exposed by the supported `wiiland-hid` source API.
//!
//! The kernel-facing constants and representations in this module are crate
//! private implementation details; they are not an ABI or API promise.

#[cfg(not(windows))]
pub(crate) const DRUM_SLOT_COUNT: usize = 8;
#[cfg(not(windows))]
pub(crate) const EV_SYN: u16 = 0;
#[cfg(not(windows))]
pub(crate) const EV_KEY: u16 = 1;
#[cfg(not(windows))]
pub(crate) const EV_ABS: u16 = 3;
#[cfg(not(windows))]
pub(crate) const SYN_REPORT: u16 = 0;
#[cfg(not(windows))]
pub(crate) const SYN_DROPPED: u16 = 3;

/// A three-axis sample or one logical slot in a multi-axis report.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Axis3 {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

/// An event timestamp owned by the Rust API.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Timestamp {
    pub seconds: i64,
    pub microseconds: u32,
}

impl Timestamp {
    #[cfg(not(windows))]
    pub(crate) fn from_timeval(time: libc::timeval) -> Self {
        #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
        let seconds = time.tv_sec;
        #[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
        let seconds = i64::from(time.tv_sec);

        Self {
            seconds,
            microseconds: u32::try_from(time.tv_usec).unwrap_or(0).min(999_999),
        }
    }
}

pub use wiiland_core::input::{Button, ButtonEvent, ButtonState};

#[cfg(not(windows))]
/// A raw Linux input event consumed by the private decoder.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct InputEvent {
    pub(crate) time: libc::timeval,
    pub(crate) event_type: u16,
    pub(crate) code: u16,
    pub(crate) value: i32,
}

#[cfg(not(windows))]
impl PartialEq for InputEvent {
    fn eq(&self, other: &Self) -> bool {
        self.time.tv_sec == other.time.tv_sec
            && self.time.tv_usec == other.time.tv_usec
            && self.event_type == other.event_type
            && self.code == other.code
            && self.value == other.value
    }
}
#[cfg(not(windows))]
impl Eq for InputEvent {}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct InterfaceMask(u32);
impl InterfaceMask {
    pub const CORE: Self = Self(0x000001);
    pub const ACCEL: Self = Self(0x000002);
    pub const IR: Self = Self(0x000004);
    pub const MOTION_PLUS: Self = Self(0x000100);
    pub const NUNCHUK: Self = Self(0x000200);
    pub const CLASSIC_CONTROLLER: Self = Self(0x000400);
    pub const BALANCE_BOARD: Self = Self(0x000800);
    pub const PRO_CONTROLLER: Self = Self(0x001000);
    pub const DRUMS: Self = Self(0x002000);
    pub const GUITAR: Self = Self(0x004000);
    pub const ALL: Self = Self(0x007f07);
    pub const WRITABLE: Self = Self(0x010000);
    pub const fn empty() -> Self {
        Self(0)
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
    #[cfg(not(windows))]
    pub(crate) const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }
    pub const fn bits(self) -> u32 {
        self.0
    }
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn insert(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    pub const fn remove(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}
impl core::ops::BitOr for InterfaceMask {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}
impl core::ops::BitOrAssign for InterfaceMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}
impl core::ops::BitAndAssign for InterfaceMask {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}
impl core::ops::BitAnd for InterfaceMask {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}
impl core::ops::Not for InterfaceMask {
    type Output = Self;
    fn not(self) -> Self {
        Self(!self.0)
    }
}

// Common evdev key/absolute codes. These stay private to the decoder.
#[cfg(not(windows))]
pub(crate) const KEY_LEFT: u16 = 105;
#[cfg(not(windows))]
pub(crate) const KEY_RIGHT: u16 = 106;
#[cfg(not(windows))]
pub(crate) const KEY_UP: u16 = 103;
#[cfg(not(windows))]
pub(crate) const KEY_DOWN: u16 = 108;
#[cfg(not(windows))]
pub(crate) const KEY_NEXT: u16 = 0x197;
#[cfg(not(windows))]
pub(crate) const KEY_PREVIOUS: u16 = 0x19c;
#[cfg(not(windows))]
pub(crate) const BTN_1: u16 = 0x101;
#[cfg(not(windows))]
pub(crate) const BTN_2: u16 = 0x102;
#[cfg(not(windows))]
pub(crate) const BTN_3: u16 = 0x103;
#[cfg(not(windows))]
pub(crate) const BTN_4: u16 = 0x104;
#[cfg(not(windows))]
pub(crate) const BTN_5: u16 = 0x105;
#[cfg(not(windows))]
pub(crate) const BTN_A: u16 = 0x130;
#[cfg(not(windows))]
pub(crate) const BTN_B: u16 = 0x131;
#[cfg(not(windows))]
pub(crate) const BTN_C: u16 = 0x132;
#[cfg(not(windows))]
pub(crate) const BTN_X: u16 = 0x133;
#[cfg(not(windows))]
pub(crate) const BTN_Y: u16 = 0x134;
#[cfg(not(windows))]
pub(crate) const BTN_Z: u16 = 0x135;
#[cfg(not(windows))]
pub(crate) const BTN_TL: u16 = 0x136;
#[cfg(not(windows))]
pub(crate) const BTN_TR: u16 = 0x137;
#[cfg(not(windows))]
pub(crate) const BTN_TL2: u16 = 0x138;
#[cfg(not(windows))]
pub(crate) const BTN_TR2: u16 = 0x139;
#[cfg(not(windows))]
pub(crate) const BTN_SELECT: u16 = 0x13a;
#[cfg(not(windows))]
pub(crate) const BTN_START: u16 = 0x13b;
#[cfg(not(windows))]
pub(crate) const BTN_MODE: u16 = 0x13c;
#[cfg(not(windows))]
pub(crate) const BTN_THUMBL: u16 = 0x13d;
#[cfg(not(windows))]
pub(crate) const BTN_THUMBR: u16 = 0x13e;
#[cfg(not(windows))]
pub(crate) const BTN_DPAD_UP: u16 = 0x220;
#[cfg(not(windows))]
pub(crate) const BTN_DPAD_DOWN: u16 = 0x221;
#[cfg(not(windows))]
pub(crate) const BTN_DPAD_LEFT: u16 = 0x222;
#[cfg(not(windows))]
pub(crate) const BTN_DPAD_RIGHT: u16 = 0x223;
#[cfg(not(windows))]
pub(crate) const BTN_EAST: u16 = BTN_B;
#[cfg(not(windows))]
pub(crate) const BTN_SOUTH: u16 = BTN_A;
#[cfg(not(windows))]
pub(crate) const BTN_NORTH: u16 = BTN_X;
#[cfg(not(windows))]
pub(crate) const BTN_WEST: u16 = BTN_Y;
#[cfg(not(windows))]
pub(crate) const ABS_X: u16 = 0;
#[cfg(not(windows))]
pub(crate) const ABS_Y: u16 = 1;
#[cfg(not(windows))]
pub(crate) const ABS_RX: u16 = 3;
#[cfg(not(windows))]
pub(crate) const ABS_RY: u16 = 4;
#[cfg(not(windows))]
pub(crate) const ABS_RZ: u16 = 5;
#[cfg(not(windows))]
pub(crate) const ABS_HAT0X: u16 = 16;
#[cfg(not(windows))]
pub(crate) const ABS_HAT0Y: u16 = 17;
#[cfg(not(windows))]
pub(crate) const ABS_HAT1X: u16 = 18;
#[cfg(not(windows))]
pub(crate) const ABS_HAT1Y: u16 = 19;
#[cfg(not(windows))]
pub(crate) const ABS_HAT2X: u16 = 20;
#[cfg(not(windows))]
pub(crate) const ABS_HAT2Y: u16 = 21;
#[cfg(not(windows))]
pub(crate) const ABS_HAT3X: u16 = 22;
#[cfg(not(windows))]
pub(crate) const ABS_HAT3Y: u16 = 23;
