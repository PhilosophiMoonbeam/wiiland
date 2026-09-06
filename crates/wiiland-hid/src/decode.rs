//! Private evdev decoder and state machine implementation.

use crate::model::{
    ABS_HAT0X, ABS_HAT0Y, ABS_HAT1X, ABS_HAT1Y, ABS_HAT2X, ABS_HAT2Y, ABS_HAT3X, ABS_HAT3Y, ABS_RX,
    ABS_RY, ABS_RZ, ABS_X, ABS_Y, Axis3, BTN_1, BTN_2, BTN_3, BTN_4, BTN_5, BTN_A, BTN_B, BTN_C,
    BTN_DPAD_DOWN, BTN_DPAD_LEFT, BTN_DPAD_RIGHT, BTN_DPAD_UP, BTN_EAST, BTN_MODE, BTN_NORTH,
    BTN_SELECT, BTN_SOUTH, BTN_START, BTN_THUMBL, BTN_THUMBR, BTN_TL, BTN_TL2, BTN_TR, BTN_TR2,
    BTN_WEST, BTN_X, BTN_Y, BTN_Z, Button, ButtonEvent, ButtonState, DRUM_SLOT_COUNT, EV_ABS,
    EV_KEY, EV_SYN, InputEvent, KEY_DOWN, KEY_LEFT, KEY_NEXT, KEY_PREVIOUS, KEY_RIGHT, KEY_UP,
    SYN_DROPPED, SYN_REPORT, Timestamp,
};
use libc::timeval;

/// Typed event discriminant with an explicit future-value case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EventType {
    Key,
    Accel,
    Ir,
    BalanceBoard,
    MotionPlus,
    ProControllerKey,
    ProControllerMove,
    Watch,
    ClassicControllerKey,
    ClassicControllerMove,
    NunchukKey,
    NunchukMove,
    DrumsKey,
    DrumsMove,
    GuitarKey,
    GuitarMove,
    Gone,
    Unknown(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub(crate) enum InterfaceKind {
    Core,
    Accel,
    Ir,
    MotionPlus,
    Nunchuk,
    Classic,
    BalanceBoard,
    Pro,
    Drums,
    Guitar,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EventKind {
    Key(ButtonEvent),
    Accel(Axis3),
    Ir([Axis3; 4]),
    BalanceBoard([Axis3; 4]),
    MotionPlus(Axis3),
    ProControllerKey(ButtonEvent),
    ProControllerMove([Axis3; 2]),
    Watch,
    ClassicControllerKey(ButtonEvent),
    ClassicControllerMove([Axis3; 3]),
    NunchukKey(ButtonEvent),
    NunchukMove([Axis3; 2]),
    DrumsKey(ButtonEvent),
    DrumsMove([Axis3; 8]),
    GuitarKey(ButtonEvent),
    GuitarMove([Axis3; 3]),
    Gone,
    Unknown(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Event {
    pub time: Timestamp,
    pub kind: EventKind,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MotionPlusNormalizer {
    offsets: Axis3,
    factor: i32,
}
impl MotionPlusNormalizer {
    pub(crate) const fn new() -> Self {
        Self {
            offsets: Axis3 { x: 0, y: 0, z: 0 },
            factor: 0,
        }
    }
    pub(crate) fn set(&mut self, x: i32, y: i32, z: i32, factor: i32) {
        self.offsets.x = scale_offset(x);
        self.offsets.y = scale_offset(y);
        self.offsets.z = scale_offset(z);
        self.factor = factor;
    }
    pub(crate) const fn values(&self) -> (i32, i32, i32, i32) {
        (
            self.offsets.x / 100,
            self.offsets.y / 100,
            self.offsets.z / 100,
            self.factor,
        )
    }
    pub(crate) fn normalize(&mut self, value: Axis3) -> Axis3 {
        Axis3 {
            x: normalize_axis(value.x, &mut self.offsets.x, self.factor),
            y: normalize_axis(value.y, &mut self.offsets.y, self.factor),
            z: normalize_axis(value.z, &mut self.offsets.z, self.factor),
        }
    }
}
fn scale_offset(value: i32) -> i32 {
    (i64::from(value) * 100).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}
fn normalize_axis(value: i32, offset: &mut i32, factor: i32) -> i32 {
    let normalized = (i64::from(value) - i64::from(*offset) / 100)
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    if normalized != 0 {
        *offset = (i64::from(*offset)
            + if normalized > 0 {
                i64::from(factor)
            } else {
                -i64::from(factor)
            })
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    }
    normalized
}
const KEY_WORDS: usize = 12; // KEY_MAX is 0x2ff on Linux (768 bits).

/// State needed to preserve deterministic `SYN_DROPPED` recovery ordering.
#[derive(Clone, Debug)]
pub(crate) struct RecoveryState {
    key_state: [u64; KEY_WORDS],
    key_pending: [u64; KEY_WORDS],
    desynced: bool,
    key_resync_pending: bool,
    report_resync_pending: bool,
    resync_time: timeval,
}

impl Default for RecoveryState {
    fn default() -> Self {
        Self {
            key_state: [0; KEY_WORDS],
            key_pending: [0; KEY_WORDS],
            desynced: false,
            key_resync_pending: false,
            report_resync_pending: false,
            resync_time: timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
        }
    }
}
impl RecoveryState {
    pub(crate) fn dropped(&mut self) {
        self.desynced = true;
    }
    pub(crate) const fn is_desynced(&self) -> bool {
        self.desynced
    }
    pub(crate) fn initialize(&mut self, keys: &[u64]) {
        self.key_pending = [0; KEY_WORDS];
        for i in 0..KEY_WORDS {
            self.key_state[i] = keys.get(i).copied().unwrap_or(0);
        }
        self.desynced = false;
        self.key_resync_pending = false;
        self.report_resync_pending = false;
    }
    pub(crate) fn seed(&mut self, keys: &[u64], time: timeval, has_abs: bool) {
        self.key_resync_pending = false;
        for i in 0..KEY_WORDS {
            let next = keys.get(i).copied().unwrap_or(0);
            self.key_pending[i] = self.key_state[i] ^ next;
            if self.key_pending[i] != 0 {
                self.key_resync_pending = true;
            }
            self.key_state[i] = next;
        }
        self.resync_time = time;
        self.report_resync_pending = has_abs;
        self.desynced = false;
    }
    pub(crate) fn next_key(&mut self) -> Option<InputEvent> {
        if !self.key_resync_pending {
            return None;
        }
        for i in 0..KEY_WORDS {
            let pending = self.key_pending[i];
            if pending == 0 {
                continue;
            }
            let bit = pending.trailing_zeros() as usize;
            self.key_pending[i] &= !(1u64 << bit);
            let code = i * 64 + bit;
            return Some(InputEvent {
                time: self.resync_time,
                event_type: EV_KEY,
                code: code as u16,
                value: if self.key_state[i] & (1u64 << bit) != 0 {
                    1
                } else {
                    0
                },
            });
        }
        self.key_resync_pending = false;
        None
    }
    pub(crate) fn take_report(&mut self) -> Option<InputEvent> {
        if !self.report_resync_pending {
            return None;
        }
        self.report_resync_pending = false;
        Some(InputEvent {
            time: self.resync_time,
            event_type: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        })
    }
    pub(crate) const fn has_pending(&self) -> bool {
        self.key_resync_pending || self.report_resync_pending
    }
    fn remember(&mut self, input: InputEvent) {
        if input.event_type != EV_KEY
            || input.code as usize >= KEY_WORDS * 64
            || !(0..=2).contains(&input.value)
        {
            return;
        }
        let word = input.code as usize / 64;
        let mask = 1u64 << (input.code as usize % 64);
        if input.value == 0 {
            self.key_state[word] &= !mask;
        } else {
            self.key_state[word] |= mask;
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CacheState {
    pub(crate) accel: Axis3,
    pub(crate) ir: [Axis3; 4],
    pub(crate) motion_plus: Axis3,
    pub(crate) nunchuk: [Axis3; 2],
    pub(crate) classic: [Axis3; 3],
    pub(crate) balance_board: [Axis3; 4],
    pub(crate) pro: [Axis3; 2],
    pub(crate) drums: [Axis3; DRUM_SLOT_COUNT],
    pub(crate) guitar: [Axis3; 3],
}
impl Default for CacheState {
    fn default() -> Self {
        let mut ir = [Axis3::default(); 4];
        for p in &mut ir {
            p.x = 1023;
            p.y = 1023;
        }
        Self {
            accel: Axis3::default(),
            ir,
            motion_plus: Axis3::default(),
            nunchuk: [Axis3::default(); 2],
            classic: [Axis3::default(); 3],
            balance_board: [Axis3::default(); 4],
            pro: [Axis3::default(); 2],
            drums: [Axis3::default(); DRUM_SLOT_COUNT],
            guitar: [Axis3::default(); 3],
        }
    }
}

pub(crate) struct Decoder {
    pub(crate) interface: InterfaceKind,
    pub(crate) cache: CacheState,
    pub(crate) recovery: RecoveryState,
    pub(crate) motion_plus: MotionPlusNormalizer,
}
impl Decoder {
    pub(crate) fn new(interface: InterfaceKind) -> Self {
        Self {
            interface,
            cache: CacheState::default(),
            recovery: RecoveryState::default(),
            motion_plus: MotionPlusNormalizer::new(),
        }
    }
    pub(crate) fn set_mp_normalization(&mut self, x: i32, y: i32, z: i32, factor: i32) {
        self.motion_plus.set(x, y, z, factor);
    }
    pub(crate) fn mp_normalization(&self) -> (i32, i32, i32, i32) {
        self.motion_plus.values()
    }
    pub(crate) fn push(&mut self, input: InputEvent) -> Option<Event> {
        if self.recovery.is_desynced() {
            if input.event_type == EV_SYN && input.code == SYN_REPORT {
                return None;
            }
            return None;
        }
        if input.event_type == EV_SYN && input.code == SYN_DROPPED {
            self.recovery.dropped();
            return None;
        }
        if input.event_type == EV_KEY {
            self.recovery.remember(input);
            return self.decode_key(input);
        }
        if input.event_type == EV_ABS {
            self.update_abs(input.code, input.value);
            return None;
        }
        if input.event_type == EV_SYN && input.code == SYN_REPORT {
            return self.report(input.time);
        }
        None
    }
    pub(crate) fn seed_state(&mut self, keys: &[u64], abs: &[(u16, i32)]) {
        for &(code, value) in abs {
            self.update_abs(code, value);
        }
        self.recovery.initialize(keys);
    }
    pub(crate) fn recover(&mut self, keys: &[u64], abs: &[(u16, i32)], time: timeval) {
        for &(code, value) in abs {
            self.update_abs(code, value);
        }
        let has_abs = !abs_codes(self.interface).is_empty();
        self.recovery.seed(keys, time, has_abs);
    }
    pub(crate) fn push_recovered(&mut self) -> Option<Event> {
        if let Some(input) = self.recovery.next_key() {
            return self.decode_key(input);
        }
        if let Some(input) = self.recovery.take_report() {
            return self.push_report(input);
        }
        None
    }
    fn push_report(&mut self, input: InputEvent) -> Option<Event> {
        self.report(input.time)
    }
    fn report(&mut self, time: timeval) -> Option<Event> {
        let kind = match self.interface {
            InterfaceKind::Accel => EventKind::Accel(self.cache.accel),
            InterfaceKind::Ir => EventKind::Ir(self.cache.ir),
            InterfaceKind::MotionPlus => {
                EventKind::MotionPlus(self.motion_plus.normalize(self.cache.motion_plus))
            }
            InterfaceKind::Nunchuk => EventKind::NunchukMove(self.cache.nunchuk),
            InterfaceKind::Classic => EventKind::ClassicControllerMove(self.cache.classic),
            InterfaceKind::BalanceBoard => EventKind::BalanceBoard(self.cache.balance_board),
            InterfaceKind::Pro => EventKind::ProControllerMove(self.cache.pro),
            InterfaceKind::Drums => EventKind::DrumsMove(self.cache.drums),
            InterfaceKind::Guitar => EventKind::GuitarMove(self.cache.guitar),
            InterfaceKind::Core => return None,
        };
        Some(Event {
            time: Timestamp::from_timeval(time),
            kind,
        })
    }
    fn decode_key(&self, input: InputEvent) -> Option<Event> {
        if !(0..=2).contains(&input.value) {
            return None;
        }
        let button = match self.interface {
            InterfaceKind::Core => map_core_key(input.code),
            InterfaceKind::Nunchuk => map_nunchuk_key(input.code),
            InterfaceKind::Classic => map_classic_key(input.code),
            InterfaceKind::Pro => map_pro_key(input.code),
            InterfaceKind::Drums => map_drums_key(input.code),
            InterfaceKind::Guitar => map_guitar_key(input.code),
            _ => None,
        }?;
        let state = match input.value {
            0 => ButtonState::Released,
            1 => ButtonState::Pressed,
            2 => ButtonState::Repeated,
            _ => return None,
        };
        let key = ButtonEvent { button, state };
        let kind = match self.interface {
            InterfaceKind::Core => EventKind::Key(key),
            InterfaceKind::Nunchuk => EventKind::NunchukKey(key),
            InterfaceKind::Classic => EventKind::ClassicControllerKey(key),
            InterfaceKind::Pro => EventKind::ProControllerKey(key),
            InterfaceKind::Drums => EventKind::DrumsKey(key),
            InterfaceKind::Guitar => EventKind::GuitarKey(key),
            _ => return None,
        };
        Some(Event {
            time: Timestamp::from_timeval(input.time),
            kind,
        })
    }
    fn update_abs(&mut self, code: u16, value: i32) {
        match self.interface {
            InterfaceKind::Accel => match code {
                ABS_RX => self.cache.accel.x = value,
                ABS_RY => self.cache.accel.y = value,
                ABS_RZ => self.cache.accel.z = value,
                _ => {}
            },
            InterfaceKind::MotionPlus => match code {
                ABS_RX => self.cache.motion_plus.x = value,
                ABS_RY => self.cache.motion_plus.y = value,
                ABS_RZ => self.cache.motion_plus.z = value,
                _ => {}
            },
            InterfaceKind::Ir => {
                let i = match code {
                    ABS_HAT0X => Some((0, true)),
                    ABS_HAT0Y => Some((0, false)),
                    ABS_HAT1X => Some((1, true)),
                    ABS_HAT1Y => Some((1, false)),
                    ABS_HAT2X => Some((2, true)),
                    ABS_HAT2Y => Some((2, false)),
                    ABS_HAT3X => Some((3, true)),
                    ABS_HAT3Y => Some((3, false)),
                    _ => None,
                };
                if let Some((i, x)) = i {
                    if x {
                        self.cache.ir[i].x = value
                    } else {
                        self.cache.ir[i].y = value
                    }
                }
            }
            InterfaceKind::Nunchuk => match code {
                ABS_HAT0X => self.cache.nunchuk[0].x = value,
                ABS_HAT0Y => self.cache.nunchuk[0].y = value,
                ABS_RX => self.cache.nunchuk[1].x = value,
                ABS_RY => self.cache.nunchuk[1].y = value,
                ABS_RZ => self.cache.nunchuk[1].z = value,
                _ => {}
            },
            InterfaceKind::Classic => match code {
                ABS_HAT1X => self.cache.classic[0].x = value,
                ABS_HAT1Y => self.cache.classic[0].y = value,
                ABS_HAT2X => self.cache.classic[1].x = value,
                ABS_HAT2Y => self.cache.classic[1].y = value,
                ABS_HAT3X => self.cache.classic[2].y = value,
                ABS_HAT3Y => self.cache.classic[2].x = value,
                _ => {}
            },
            InterfaceKind::BalanceBoard => match code {
                ABS_HAT0X => self.cache.balance_board[0].x = value,
                ABS_HAT0Y => self.cache.balance_board[1].x = value,
                ABS_HAT1X => self.cache.balance_board[2].x = value,
                ABS_HAT1Y => self.cache.balance_board[3].x = value,
                _ => {}
            },
            InterfaceKind::Pro => match code {
                ABS_X => self.cache.pro[0].x = value,
                ABS_Y => self.cache.pro[0].y = value,
                ABS_RX => self.cache.pro[1].x = value,
                ABS_RY => self.cache.pro[1].y = value,
                _ => {}
            },
            InterfaceKind::Drums => match code {
                ABS_X => self.cache.drums[0].x = value,
                ABS_Y => self.cache.drums[0].y = value,
                ABS_HAT2X => self.cache.drums[1].x = value,
                ABS_HAT2Y => self.cache.drums[2].x = value,
                ABS_HAT0X => self.cache.drums[3].x = value,
                ABS_HAT1X => self.cache.drums[4].x = value,
                ABS_HAT0Y => self.cache.drums[5].x = value,
                ABS_HAT3X => self.cache.drums[6].x = value,
                ABS_HAT3Y => self.cache.drums[7].x = value,
                _ => {}
            },
            InterfaceKind::Guitar => match code {
                ABS_X => self.cache.guitar[0].x = value,
                ABS_Y => self.cache.guitar[0].y = value,
                ABS_HAT1X => self.cache.guitar[1].x = value,
                ABS_HAT0X => self.cache.guitar[2].x = value,
                _ => {}
            },
            InterfaceKind::Core => {}
        }
    }
}

pub(crate) fn abs_codes(interface: InterfaceKind) -> &'static [u16] {
    match interface {
        InterfaceKind::Accel | InterfaceKind::MotionPlus => &[ABS_RX, ABS_RY, ABS_RZ],
        InterfaceKind::Ir => &[
            ABS_HAT0X, ABS_HAT0Y, ABS_HAT1X, ABS_HAT1Y, ABS_HAT2X, ABS_HAT2Y, ABS_HAT3X, ABS_HAT3Y,
        ],
        InterfaceKind::Nunchuk => &[ABS_HAT0X, ABS_HAT0Y, ABS_RX, ABS_RY, ABS_RZ],
        InterfaceKind::Classic => &[
            ABS_HAT1X, ABS_HAT1Y, ABS_HAT2X, ABS_HAT2Y, ABS_HAT3X, ABS_HAT3Y,
        ],
        InterfaceKind::BalanceBoard => &[ABS_HAT0X, ABS_HAT0Y, ABS_HAT1X, ABS_HAT1Y],
        InterfaceKind::Pro => &[ABS_X, ABS_Y, ABS_RX, ABS_RY],
        InterfaceKind::Drums => &[
            ABS_X, ABS_Y, ABS_HAT2X, ABS_HAT2Y, ABS_HAT0X, ABS_HAT1X, ABS_HAT0Y, ABS_HAT3X,
            ABS_HAT3Y,
        ],
        InterfaceKind::Guitar => &[ABS_X, ABS_Y, ABS_HAT1X, ABS_HAT0X],
        InterfaceKind::Core => &[],
    }
}

fn map_core_key(c: u16) -> Option<Button> {
    Some(match c {
        KEY_LEFT => Button::Left,
        KEY_RIGHT => Button::Right,
        KEY_UP => Button::Up,
        KEY_DOWN => Button::Down,
        KEY_NEXT => Button::Plus,
        KEY_PREVIOUS => Button::Minus,
        BTN_1 => Button::One,
        BTN_2 => Button::Two,
        BTN_A => Button::A,
        BTN_B => Button::B,
        BTN_MODE => Button::Home,
        _ => return None,
    })
}
fn map_nunchuk_key(c: u16) -> Option<Button> {
    Some(match c {
        BTN_C => Button::C,
        BTN_Z => Button::Z,
        _ => return None,
    })
}
fn map_classic_key(c: u16) -> Option<Button> {
    Some(match c {
        BTN_A => Button::A,
        BTN_B => Button::B,
        BTN_X => Button::X,
        BTN_Y => Button::Y,
        KEY_NEXT => Button::Plus,
        KEY_PREVIOUS => Button::Minus,
        BTN_MODE => Button::Home,
        KEY_LEFT => Button::Left,
        KEY_RIGHT => Button::Right,
        KEY_UP => Button::Up,
        KEY_DOWN => Button::Down,
        BTN_TL => Button::ShoulderLeft,
        BTN_TR => Button::ShoulderRight,
        BTN_TL2 => Button::TriggerLeft,
        BTN_TR2 => Button::TriggerRight,
        _ => return None,
    })
}
fn map_pro_key(c: u16) -> Option<Button> {
    Some(match c {
        BTN_EAST => Button::A,
        BTN_SOUTH => Button::B,
        BTN_NORTH => Button::X,
        BTN_WEST => Button::Y,
        BTN_START => Button::Plus,
        BTN_SELECT => Button::Minus,
        BTN_MODE => Button::Home,
        BTN_DPAD_LEFT => Button::Left,
        BTN_DPAD_RIGHT => Button::Right,
        BTN_DPAD_UP => Button::Up,
        BTN_DPAD_DOWN => Button::Down,
        BTN_TL => Button::ShoulderLeft,
        BTN_TR => Button::ShoulderRight,
        BTN_TL2 => Button::TriggerLeft,
        BTN_TR2 => Button::TriggerRight,
        BTN_THUMBL => Button::ThumbLeft,
        BTN_THUMBR => Button::ThumbRight,
        _ => return None,
    })
}
fn map_drums_key(c: u16) -> Option<Button> {
    Some(match c {
        BTN_START => Button::Plus,
        BTN_SELECT => Button::Minus,
        _ => return None,
    })
}
fn map_guitar_key(c: u16) -> Option<Button> {
    Some(match c {
        BTN_1 => Button::FretFarUp,
        BTN_2 => Button::FretUp,
        BTN_3 => Button::FretMid,
        BTN_4 => Button::FretLow,
        BTN_5 => Button::FretFarLow,
        BTN_DPAD_UP => Button::StrumBarUp,
        BTN_DPAD_DOWN => Button::StrumBarDown,
        BTN_START => Button::Plus,
        BTN_SELECT => Button::Minus,
        _ => return None,
    })
}

impl EventKind {
    /// Readable interface that produced this report, absent for lifecycle events.
    pub fn interface(self) -> Option<crate::model::InterfaceMask> {
        use crate::model::InterfaceMask as M;
        Some(match self {
            Self::Key(_) => M::CORE,
            Self::Accel(_) => M::ACCEL,
            Self::Ir(_) => M::IR,
            Self::BalanceBoard(_) => M::BALANCE_BOARD,
            Self::MotionPlus(_) => M::MOTION_PLUS,
            Self::ProControllerKey(_) | Self::ProControllerMove(_) => M::PRO_CONTROLLER,
            Self::ClassicControllerKey(_) | Self::ClassicControllerMove(_) => M::CLASSIC_CONTROLLER,
            Self::NunchukKey(_) | Self::NunchukMove(_) => M::NUNCHUK,
            Self::DrumsKey(_) | Self::DrumsMove(_) => M::DRUMS,
            Self::GuitarKey(_) | Self::GuitarMove(_) => M::GUITAR,
            _ => return None,
        })
    }

    pub const fn event_type(self) -> EventType {
        match self {
            EventKind::Key(_) => EventType::Key,
            EventKind::Accel(_) => EventType::Accel,
            EventKind::Ir(_) => EventType::Ir,
            EventKind::BalanceBoard(_) => EventType::BalanceBoard,
            EventKind::MotionPlus(_) => EventType::MotionPlus,
            EventKind::ProControllerKey(_) => EventType::ProControllerKey,
            EventKind::ProControllerMove(_) => EventType::ProControllerMove,
            EventKind::Watch => EventType::Watch,
            EventKind::ClassicControllerKey(_) => EventType::ClassicControllerKey,
            EventKind::ClassicControllerMove(_) => EventType::ClassicControllerMove,
            EventKind::NunchukKey(_) => EventType::NunchukKey,
            EventKind::NunchukMove(_) => EventType::NunchukMove,
            EventKind::DrumsKey(_) => EventType::DrumsKey,
            EventKind::DrumsMove(_) => EventType::DrumsMove,
            EventKind::GuitarKey(_) => EventType::GuitarKey,
            EventKind::GuitarMove(_) => EventType::GuitarMove,
            EventKind::Gone => EventType::Gone,
            EventKind::Unknown(value) => EventType::Unknown(value),
        }
    }
}
impl EventType {
    pub const fn code(self) -> u32 {
        match self {
            Self::Key => 0,
            Self::Accel => 1,
            Self::Ir => 2,
            Self::BalanceBoard => 3,
            Self::MotionPlus => 4,
            Self::ProControllerKey => 5,
            Self::ProControllerMove => 6,
            Self::Watch => 7,
            Self::ClassicControllerKey => 8,
            Self::ClassicControllerMove => 9,
            Self::NunchukKey => 10,
            Self::NunchukMove => 11,
            Self::DrumsKey => 12,
            Self::DrumsMove => 13,
            Self::GuitarKey => 14,
            Self::GuitarMove => 15,
            Self::Gone => 16,
            Self::Unknown(value) => value,
        }
    }
}
