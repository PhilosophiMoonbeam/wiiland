use crate::decode::EventKind;
use crate::model::{Axis3, Button, ButtonEvent, ButtonState, InterfaceMask};
use std::io;

pub(crate) const REPORT_STATUS: u8 = 0x20;
pub(crate) const REPORT_MEMORY: u8 = 0x21;
pub(crate) const REPORT_ACK: u8 = 0x22;
pub(crate) const REPORT_MODE_OUTPUT: u8 = 0x12;
pub(crate) const REPORT_LED_OUTPUT: u8 = 0x11;
pub(crate) const REPORT_STATUS_OUTPUT: u8 = 0x15;
pub(crate) const REPORT_MEMORY_READ_OUTPUT: u8 = 0x17;
pub(crate) const REPORT_MEMORY_WRITE_OUTPUT: u8 = 0x16;
pub(crate) const REPORT_IR_ENABLE_1: u8 = 0x13;
pub(crate) const REPORT_IR_ENABLE_2: u8 = 0x1a;
pub(crate) const REPORT_OUTPUT_COMMON_RUMBLE: u8 = 0x01;
pub(crate) const REPORT_OUTPUT_COMMON_ACK: u8 = 0x02;
pub(crate) const REPORT_OUTPUT_COMMON_ENABLE: u8 = 0x04;
pub(crate) const REPORT_OUTPUT_REGISTERS: u8 = 0x04;
pub(crate) const REPORT_MODE_CORE: u8 = 0x30;
pub(crate) const REPORT_MODE_CORE_ACCEL: u8 = 0x31;
pub(crate) const REPORT_MODE_CORE_EXTENSION_8: u8 = 0x32;
pub(crate) const REPORT_MODE_CORE_ACCEL_IR_12: u8 = 0x33;
pub(crate) const REPORT_MODE_CORE_EXTENSION_19: u8 = 0x34;
pub(crate) const REPORT_MODE_CORE_ACCEL_EXTENSION_16: u8 = 0x35;
pub(crate) const REPORT_MODE_CORE_IR_10_EXTENSION_9: u8 = 0x36;
pub(crate) const REPORT_MODE_CORE_ACCEL_IR_10_EXTENSION_6: u8 = 0x37;
pub(crate) const EEPROM_ACCEL_CALIBRATION: u32 = 0x0016;
pub(crate) const EXTENSION_REGISTER_BASE: u32 = 0xa40000;
pub(crate) const MOTION_PLUS_REGISTER_BASE: u32 = 0xa60000;
pub(crate) const IR_REGISTER_BASE: u32 = 0xb00000;

const BUTTON_MAP_NUNCHUK: [Button; 2] = [Button::C, Button::Z];
const BUTTON_MAP_CLASSIC: [Button; 15] = [
    Button::A,
    Button::B,
    Button::X,
    Button::Y,
    Button::Plus,
    Button::Minus,
    Button::Home,
    Button::Left,
    Button::Right,
    Button::Up,
    Button::Down,
    Button::ShoulderLeft,
    Button::ShoulderRight,
    Button::TriggerLeft,
    Button::TriggerRight,
];
const BUTTON_MAP_PRO: [Button; 17] = [
    Button::A,
    Button::B,
    Button::X,
    Button::Y,
    Button::Plus,
    Button::Minus,
    Button::Home,
    Button::Left,
    Button::Right,
    Button::Up,
    Button::Down,
    Button::ShoulderLeft,
    Button::ShoulderRight,
    Button::TriggerLeft,
    Button::TriggerRight,
    Button::ThumbLeft,
    Button::ThumbRight,
];
const BUTTON_MAP_GUITAR: [Button; 9] = [
    Button::FretFarUp,
    Button::FretUp,
    Button::FretMid,
    Button::FretLow,
    Button::FretFarLow,
    Button::StrumBarUp,
    Button::StrumBarDown,
    Button::Plus,
    Button::Minus,
];
const BUTTON_MAP_DRUMS: [Button; 2] = [Button::Plus, Button::Minus];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtensionKind {
    None,
    Unknown,
    Nunchuk,
    Classic,
    Pro,
    BalanceBoard,
    Guitar,
    Drums,
    MotionPlus,
}

impl ExtensionKind {
    pub(crate) const fn interface(self) -> Option<InterfaceMask> {
        Some(match self {
            Self::Nunchuk => InterfaceMask::NUNCHUK,
            Self::Classic => InterfaceMask::CLASSIC_CONTROLLER,
            Self::Pro => InterfaceMask::PRO_CONTROLLER,
            Self::BalanceBoard => InterfaceMask::BALANCE_BOARD,
            Self::Guitar => InterfaceMask::GUITAR,
            Self::Drums => InterfaceMask::DRUMS,
            Self::MotionPlus => InterfaceMask::MOTION_PLUS,
            Self::None | Self::Unknown => return None,
        })
    }

    pub(crate) const fn button_map(self) -> &'static [Button] {
        match self {
            Self::Nunchuk => &BUTTON_MAP_NUNCHUK,
            Self::Classic => &BUTTON_MAP_CLASSIC,
            Self::Pro => &BUTTON_MAP_PRO,
            Self::Guitar => &BUTTON_MAP_GUITAR,
            Self::Drums => &BUTTON_MAP_DRUMS,
            Self::None | Self::Unknown | Self::BalanceBoard | Self::MotionPlus => &[],
        }
    }

    const fn needs_long_report(self) -> bool {
        matches!(self, Self::BalanceBoard | Self::Pro | Self::Unknown)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ModePlan {
    modes: [u8; 2],
    len: usize,
}

impl ModePlan {
    const fn one(mode: u8) -> Self {
        Self {
            modes: [mode, 0],
            len: 1,
        }
    }

    const fn alternating(first: u8, second: u8) -> Self {
        Self {
            modes: [first, second],
            len: 2,
        }
    }

    pub(crate) const fn first(self) -> u8 {
        self.modes[0]
    }

    pub(crate) fn next_after(self, current: u8) -> Option<u8> {
        if self.len != 2 {
            return None;
        }
        if current == self.modes[0] {
            Some(self.modes[1])
        } else {
            Some(self.modes[0])
        }
    }

    pub(crate) const fn contains(self, mode: u8) -> bool {
        self.modes[0] == mode || (self.len == 2 && self.modes[1] == mode)
    }
}

pub(crate) fn report_modes(opened: InterfaceMask, extension: ExtensionKind) -> Option<ModePlan> {
    if opened.is_empty() {
        return None;
    }
    let accel = opened.contains(InterfaceMask::ACCEL);
    let ir = opened.contains(InterfaceMask::IR);
    let extension_requested = opened.contains(InterfaceMask::MOTION_PLUS)
        || opened.contains(InterfaceMask::NUNCHUK)
        || opened.contains(InterfaceMask::CLASSIC_CONTROLLER)
        || opened.contains(InterfaceMask::BALANCE_BOARD)
        || opened.contains(InterfaceMask::PRO_CONTROLLER)
        || opened.contains(InterfaceMask::DRUMS)
        || opened.contains(InterfaceMask::GUITAR);
    if ir && accel && extension_requested && extension.needs_long_report() {
        return Some(ModePlan::alternating(
            REPORT_MODE_CORE_ACCEL_IR_12,
            REPORT_MODE_CORE_ACCEL_EXTENSION_16,
        ));
    }
    Some(ModePlan::one(if ir && accel && extension_requested {
        REPORT_MODE_CORE_ACCEL_IR_10_EXTENSION_6
    } else if ir && extension_requested {
        REPORT_MODE_CORE_IR_10_EXTENSION_9
    } else if ir {
        REPORT_MODE_CORE_ACCEL_IR_12
    } else if accel && extension_requested {
        REPORT_MODE_CORE_ACCEL_EXTENSION_16
    } else if extension_requested {
        REPORT_MODE_CORE_EXTENSION_19
    } else if accel {
        REPORT_MODE_CORE_ACCEL
    } else {
        REPORT_MODE_CORE
    }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StatusReport {
    pub(crate) buttons: u16,
    pub(crate) flags: u8,
    pub(crate) battery: u8,
}

impl StatusReport {
    pub(crate) const fn extension_connected(self) -> bool {
        self.flags & 0x02 != 0
    }

    pub(crate) const fn led_mask(self) -> u8 {
        (self.flags >> 4) & 0x0f
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MemoryReport {
    pub(crate) buttons: u16,
    pub(crate) offset: u16,
    pub(crate) error: u8,
    pub(crate) bytes: [u8; 16],
    pub(crate) len: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AckReport {
    pub(crate) buttons: u16,
    pub(crate) report_id: u8,
    pub(crate) error: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExtensionFrame {
    pub(crate) buttons: u32,
    pub(crate) movement: EventKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DataFrame {
    pub(crate) buttons: Option<u16>,
    pub(crate) accel: Option<Axis3>,
    pub(crate) ir: Option<[Axis3; 4]>,
    pub(crate) extension: Option<ExtensionFrame>,
    pub(crate) motion_plus_extension_connected: Option<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AccelCalibration {
    pub(crate) zero: Axis3,
}

impl AccelCalibration {
    pub(crate) fn from_eeprom(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 10
            || bytes[9]
                != bytes[..9]
                    .iter()
                    .fold(0x55u8, |sum, byte| sum.wrapping_add(*byte))
        {
            return None;
        }
        Some(Self {
            zero: Axis3 {
                x: (i32::from(bytes[0]) << 2) | i32::from((bytes[3] >> 4) & 0x03),
                y: (i32::from(bytes[1]) << 2) | i32::from((bytes[3] >> 2) & 0x03),
                z: (i32::from(bytes[2]) << 2) | i32::from(bytes[3] & 0x03),
            },
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BalanceCalibration {
    points: [[u16; 3]; 4],
}

impl BalanceCalibration {
    pub(crate) fn from_bytes(bytes: [u8; 24]) -> Self {
        Self {
            points: std::array::from_fn(|corner| {
                std::array::from_fn(|point| {
                    let offset = (point * 4 + corner) * 2;
                    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
                })
            }),
        }
    }

    fn weight(&self, corner: usize, raw: u16) -> i32 {
        let [zero, seventeen_kg, thirty_four_kg] = self.points[corner];
        let raw = i32::from(raw);
        let zero = i32::from(zero);
        let seventeen_kg = i32::from(seventeen_kg);
        let thirty_four_kg = i32::from(thirty_four_kg);
        if raw <= zero {
            0
        } else if raw < seventeen_kg {
            let range = seventeen_kg - zero;
            let range = if range == 0 { 1 } else { range };
            (raw - zero) * 1700 / range
        } else {
            let range = thirty_four_kg - seventeen_kg;
            let range = if range == 0 { 1 } else { range };
            1700 + (raw - seventeen_kg) * 1700 / range
        }
    }
}

pub(crate) fn parse_status(report: &[u8]) -> io::Result<StatusReport> {
    if report.first() != Some(&REPORT_STATUS) || report.len() < 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated Wii Remote status report",
        ));
    }
    Ok(StatusReport {
        buttons: decode_button_bits(report[1], report[2]),
        flags: report[3],
        battery: report[7],
    })
}

pub(crate) fn parse_memory(report: &[u8]) -> io::Result<MemoryReport> {
    if report.first() != Some(&REPORT_MEMORY) || report.len() < 22 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated Wii Remote memory report",
        ));
    }
    let size = usize::from(report[3] >> 4) + 1;
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&report[6..22]);
    Ok(MemoryReport {
        buttons: decode_button_bits(report[1], report[2]),
        offset: u16::from_be_bytes([report[4], report[5]]),
        error: report[3] & 0x0f,
        bytes,
        len: size,
    })
}

pub(crate) fn parse_ack(report: &[u8]) -> io::Result<AckReport> {
    if report.first() != Some(&REPORT_ACK) || report.len() < 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated Wii Remote acknowledgement report",
        ));
    }
    Ok(AckReport {
        buttons: decode_button_bits(report[1], report[2]),
        report_id: report[3],
        error: report[4],
    })
}

pub(crate) fn memory_read_packet(
    address: u32,
    length: u16,
    registers: bool,
    rumble: bool,
) -> [u8; 7] {
    let flags = (if rumble {
        REPORT_OUTPUT_COMMON_RUMBLE
    } else {
        0
    }) | (if registers {
        REPORT_OUTPUT_REGISTERS
    } else {
        0
    });
    let address = address & 0x00ff_ffff;
    let [_, high, middle, low] = address.to_be_bytes();
    let [length_high, length_low] = length.to_be_bytes();
    [
        REPORT_MEMORY_READ_OUTPUT,
        flags,
        high,
        middle,
        low,
        length_high,
        length_low,
    ]
}

pub(crate) fn memory_write_packet(
    address: u32,
    data: &[u8],
    registers: bool,
    rumble: bool,
) -> io::Result<[u8; 22]> {
    if data.is_empty() || data.len() > 16 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Wii Remote memory writes must contain 1..=16 bytes",
        ));
    }
    let address = address & 0x00ff_ffff;
    let [_, high, middle, low] = address.to_be_bytes();
    let common = (if rumble {
        REPORT_OUTPUT_COMMON_RUMBLE
    } else {
        0
    }) | REPORT_OUTPUT_COMMON_ACK
        | (if registers {
            REPORT_OUTPUT_REGISTERS
        } else {
            0
        });
    let mut packet = [0; 22];
    packet[0] = REPORT_MEMORY_WRITE_OUTPUT;
    packet[1] = common;
    packet[2] = high;
    packet[3] = middle;
    packet[4] = low;
    packet[5] = data.len() as u8;
    packet[6..6 + data.len()].copy_from_slice(data);
    Ok(packet)
}

pub(crate) fn classify_extension_id(id: &[u8]) -> ExtensionKind {
    match id {
        [0x00, 0x00, 0xa4, 0x20, 0x00, 0x00] => ExtensionKind::Nunchuk,
        [0x00, 0x00, 0xa4, 0x20, 0x01, 0x01] => ExtensionKind::Classic,
        [0x01, 0x00, 0xa4, 0x20, 0x01, 0x20] => ExtensionKind::Pro,
        [0x00, 0x00, 0xa4, 0x20, 0x04, 0x02] => ExtensionKind::BalanceBoard,
        [0x00, 0x00, 0xa4, 0x20, 0x01, 0x03] => ExtensionKind::Guitar,
        [0x01, 0x00, 0xa4, 0x20, 0x01, 0x03] => ExtensionKind::Drums,
        [0x00, 0x00, 0xa4, 0x20, 0x04, 0x05] => ExtensionKind::MotionPlus,
        _ => ExtensionKind::Unknown,
    }
}

pub(crate) fn is_motion_plus_id(id: &[u8]) -> bool {
    matches!(
        id,
        [0x00, 0x00, 0xa6, 0x20, 0x00, 0x05]
            | [0x00, 0x00, 0xa6, 0x20, 0x04, 0x05]
            | [0x00, 0x00, 0xa6, 0x20, 0x05, 0x05]
            | [0x00, 0x00, 0xa6, 0x20, 0x07, 0x05]
            | [0x01, 0x00, 0xa4, 0x20, 0x00, 0x05]
    )
}

pub(crate) fn decode_data(
    report: &[u8],
    extension_kind: ExtensionKind,
    motion_plus_active: bool,
    balance_calibration: Option<&BalanceCalibration>,
) -> io::Result<Option<DataFrame>> {
    let Some(&id) = report.first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty Wii Remote input report",
        ));
    };
    let need = match id {
        REPORT_MODE_CORE => 3,
        REPORT_MODE_CORE_ACCEL => 6,
        REPORT_MODE_CORE_EXTENSION_8 => 11,
        REPORT_MODE_CORE_ACCEL_IR_12 => 18,
        REPORT_MODE_CORE_EXTENSION_19 => 22,
        REPORT_MODE_CORE_ACCEL_EXTENSION_16 => 22,
        REPORT_MODE_CORE_IR_10_EXTENSION_9 => 22,
        REPORT_MODE_CORE_ACCEL_IR_10_EXTENSION_6 => 22,
        _ => return Ok(None),
    };
    if report.len() < need {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated Wii Remote data report",
        ));
    }

    let buttons = decode_button_bits(report[1], report[2]);
    let (accel, ir, extension_bytes): (Option<Axis3>, Option<[Axis3; 4]>, Option<&[u8]>) = match id
    {
        REPORT_MODE_CORE => (None, None, None),
        REPORT_MODE_CORE_ACCEL => (
            Some(decode_accel(report[1], report[2], &report[3..6])),
            None,
            None,
        ),
        REPORT_MODE_CORE_EXTENSION_8 => (None, None, Some(&report[3..11])),
        REPORT_MODE_CORE_ACCEL_IR_12 => (
            Some(decode_accel(report[1], report[2], &report[3..6])),
            Some(decode_ir_extended(&report[6..18])),
            None,
        ),
        REPORT_MODE_CORE_EXTENSION_19 => (None, None, Some(&report[3..22])),
        REPORT_MODE_CORE_ACCEL_EXTENSION_16 => (
            Some(decode_accel(report[1], report[2], &report[3..6])),
            None,
            Some(&report[6..22]),
        ),
        REPORT_MODE_CORE_IR_10_EXTENSION_9 => (
            None,
            Some(decode_ir_basic(&report[3..13])),
            Some(&report[13..22]),
        ),
        REPORT_MODE_CORE_ACCEL_IR_10_EXTENSION_6 => (
            Some(decode_accel(report[1], report[2], &report[3..6])),
            Some(decode_ir_basic(&report[6..16])),
            Some(&report[16..22]),
        ),
        _ => unreachable!(),
    };
    let (extension, motion_plus_extension_connected) = if let Some(bytes) = extension_bytes {
        decode_extension(
            bytes,
            extension_kind,
            motion_plus_active,
            balance_calibration,
        )
    } else {
        (None, None)
    };
    Ok(Some(DataFrame {
        buttons: Some(buttons),
        accel,
        ir,
        extension,
        motion_plus_extension_connected,
    }))
}

fn decode_extension(
    bytes: &[u8],
    kind: ExtensionKind,
    motion_plus_active: bool,
    balance_calibration: Option<&BalanceCalibration>,
) -> (Option<ExtensionFrame>, Option<bool>) {
    if motion_plus_active && bytes.len() >= 6 {
        if bytes[5] & 0x01 != 0 {
            return (None, None);
        }
        if bytes[5] & 0x02 != 0 {
            return (
                Some(ExtensionFrame {
                    buttons: 0,
                    movement: EventKind::MotionPlus(Axis3 {
                        x: decode_motion_plus_axis(bytes[0], bytes[3], bytes[3] & 0x02 != 0),
                        y: decode_motion_plus_axis(bytes[1], bytes[4], bytes[4] & 0x02 != 0),
                        z: decode_motion_plus_axis(bytes[2], bytes[5], bytes[3] & 0x01 != 0),
                    }),
                }),
                Some(bytes[4] & 1 != 0),
            );
        }
        let frame = match kind {
            ExtensionKind::Nunchuk => decode_nunchuk_passthrough(bytes),
            ExtensionKind::Classic => decode_classic_passthrough(bytes),
            ExtensionKind::Guitar => decode_guitar_passthrough(bytes),
            ExtensionKind::Drums => decode_drums_passthrough(bytes),
            _ => None,
        };
        return (frame, Some(bytes[4] & 1 != 0));
    }

    let frame = match kind {
        ExtensionKind::Nunchuk if bytes.len() >= 6 => decode_nunchuk(bytes),
        ExtensionKind::Classic if bytes.len() >= 6 => decode_classic(bytes),
        ExtensionKind::Pro if bytes.len() >= 11 => decode_pro(bytes),
        ExtensionKind::BalanceBoard => balance_calibration
            .and_then(|calibration| decode_balance_board(bytes, calibration))
            .map(|sensors| ExtensionFrame {
                buttons: 0,
                movement: EventKind::BalanceBoard(sensors),
            }),
        ExtensionKind::Guitar if bytes.len() >= 6 => decode_guitar(bytes),
        ExtensionKind::Drums if bytes.len() >= 6 => decode_drums(bytes),
        ExtensionKind::MotionPlus => None,
        _ => None,
    };
    (frame, None)
}

fn decode_motion_plus_axis(low: u8, high: u8, slow: bool) -> i32 {
    let raw = i32::from(low) | (i32::from(high & 0xfc) << 6);
    let centered = raw - 8192;
    if slow {
        centered * 9
    } else {
        centered * 2000 * 9 / 440
    }
}

fn decode_nunchuk(data: &[u8]) -> Option<ExtensionFrame> {
    let accel = Axis3 {
        x: (i32::from(data[2]) << 2) | i32::from((data[5] >> 2) & 0x03),
        y: (i32::from(data[3]) << 2) | i32::from((data[5] >> 4) & 0x03),
        z: (i32::from(data[4]) << 2) | i32::from((data[5] >> 6) & 0x03),
    };
    let buttons = (u32::from(data[5] & 0x02 == 0)) | (u32::from(data[5] & 0x01 == 0) << 1);
    Some(ExtensionFrame {
        buttons,
        movement: EventKind::NunchukMove([
            Axis3 {
                x: i32::from(data[0]) - 128,
                y: i32::from(data[1]) - 128,
                z: 0,
            },
            Axis3 {
                x: accel.x - 0x200,
                y: accel.y - 0x200,
                z: accel.z - 0x200,
            },
        ]),
    })
}
fn decode_nunchuk_passthrough(data: &[u8]) -> Option<ExtensionFrame> {
    let mut decoded = [0; 6];
    decoded.copy_from_slice(&data[..6]);
    decoded[4] = (data[4] & 0xfe) | (data[5] >> 7);
    decoded[5] = ((data[5] & 0x40) << 1)
        | (data[5] & 0x20)
        | ((data[5] & 0x10) >> 1)
        | ((data[5] & 0x0c) >> 2);
    decode_nunchuk(&decoded)
}

fn decode_classic(data: &[u8]) -> Option<ExtensionFrame> {
    let left = Axis3 {
        x: i32::from(data[0] & 0x3f) - 32,
        y: i32::from(data[1] & 0x3f) - 32,
        z: 0,
    };
    let right_x =
        (i32::from(((data[0] >> 3) & 0x18) | ((data[1] >> 5) & 0x06) | ((data[2] >> 7) & 1)) << 1)
            - 32;
    let right = Axis3 {
        x: right_x,
        y: (i32::from(data[2] & 0x1f) << 1) - 32,
        z: 0,
    };
    let right_trigger = i32::from(data[3] & 0x1f) << 1;
    let left_trigger = i32::from(((((data[2] >> 5) & 0x03) << 3) | ((data[3] >> 5) & 0x07)) << 1);
    let buttons = decode_classic_buttons(data[4], data[5]);
    Some(ExtensionFrame {
        buttons,
        movement: EventKind::ClassicControllerMove([
            left,
            right,
            Axis3 {
                x: left_trigger,
                y: right_trigger,
                z: 0,
            },
        ]),
    })
}
fn decode_classic_passthrough(data: &[u8]) -> Option<ExtensionFrame> {
    let mut decoded = [0; 6];
    decoded.copy_from_slice(&data[..6]);
    decoded[0] &= 0xfe;
    decoded[1] &= 0xfe;
    decoded[5] = (data[5] & 0xfc) | ((data[1] & 1) << 1) | (data[0] & 1);
    decode_classic(&decoded)
}

fn decode_pro(data: &[u8]) -> Option<ExtensionFrame> {
    let axis = |low, high| i32::from(low) | (i32::from(high & 0x0f) << 8);
    let mut buttons = 0;
    let button_bits = [
        (9, 4),
        (9, 6),
        (9, 3),
        (9, 5),
        (8, 2),
        (8, 4),
        (8, 3),
        (9, 1),
        (8, 7),
        (9, 0),
        (8, 6),
        (8, 5),
        (8, 1),
        (9, 7),
        (9, 2),
        (10, 1),
        (10, 0),
    ];
    for (index, (byte, bit)) in button_bits.into_iter().enumerate() {
        if data[byte] & (1 << bit) == 0 {
            buttons |= 1 << index;
        }
    }
    let left_x = axis(data[0], data[1]) - 0x800;
    let left_y = 0x800 - axis(data[4], data[5]);
    let right_x = axis(data[2], data[3]) - 0x800;
    let right_y = 0x800 - axis(data[6], data[7]);
    Some(ExtensionFrame {
        buttons,
        movement: EventKind::ProControllerMove([
            Axis3 {
                x: left_x,
                y: left_y,
                z: 0,
            },
            Axis3 {
                x: right_x,
                y: right_y,
                z: 0,
            },
        ]),
    })
}

fn decode_classic_buttons(first: u8, second: u8) -> u32 {
    let raw = [first, second];
    let bits = [
        (1, 4),
        (1, 6),
        (1, 3),
        (1, 5),
        (0, 2),
        (0, 4),
        (0, 3),
        (1, 1),
        (0, 7),
        (1, 0),
        (0, 6),
        (0, 5),
        (0, 1),
        (1, 7),
        (1, 2),
    ];
    let mut buttons = 0;
    for (index, (byte, bit)) in bits.into_iter().enumerate() {
        if raw[byte] & (1 << bit) == 0 {
            buttons |= 1 << index;
        }
    }
    buttons
}

pub(crate) fn decode_balance_board(
    data: &[u8],
    calibration: &BalanceCalibration,
) -> Option<[Axis3; 4]> {
    if data.len() < 8 {
        return None;
    }
    Some(std::array::from_fn(|corner| {
        let offset = corner * 2;
        Axis3 {
            x: calibration.weight(corner, u16::from_be_bytes([data[offset], data[offset + 1]])),
            y: 0,
            z: 0,
        }
    }))
}

fn decode_guitar(data: &[u8]) -> Option<ExtensionFrame> {
    let mut buttons = 0;
    let fret_bits = [
        (5, 7), // orange
        (5, 6), // red
        (5, 5), // blue
        (5, 4), // green
        (5, 3), // yellow
    ];
    for (index, (byte, bit)) in fret_bits.into_iter().enumerate() {
        if data[byte] & (1 << bit) == 0 {
            buttons |= 1 << index;
        }
    }
    if data[5] & 1 == 0 {
        buttons |= 1 << 5;
    }
    if data[4] & 0x40 == 0 {
        buttons |= 1 << 6;
    }
    if data[4] & 0x04 == 0 {
        buttons |= 1 << 7;
    }
    if data[4] & 0x10 == 0 {
        buttons |= 1 << 8;
    }
    Some(ExtensionFrame {
        buttons,
        movement: EventKind::GuitarMove([
            Axis3 {
                x: i32::from(data[0] & 0x3f) - 32,
                y: i32::from(data[1] & 0x3f) - 32,
                z: 0,
            },
            Axis3 {
                x: i32::from(data[3] & 0x1f) - 16,
                y: 0,
                z: 0,
            },
            Axis3 {
                x: i32::from(data[2] & 0x1f),
                y: 0,
                z: 0,
            },
        ]),
    })
}
fn decode_guitar_passthrough(data: &[u8]) -> Option<ExtensionFrame> {
    let mut decoded = [0; 6];
    decoded.copy_from_slice(&data[..6]);
    decoded[0] &= 0xfe;
    decoded[1] &= 0xfe;
    decoded[5] = (data[5] & 0xfe) | (data[0] & 1);
    decode_guitar(&decoded)
}

fn decode_drums(data: &[u8]) -> Option<ExtensionFrame> {
    decode_drums_report(data, false)
}

fn decode_drums_report(data: &[u8], motion_plus_passthrough: bool) -> Option<ExtensionFrame> {
    let mut buttons = 0;
    if data[4] & 0x04 == 0 {
        buttons |= 1;
    }
    if data[4] & 0x10 == 0 {
        buttons |= 1 << 1;
    }

    let mut stick_x = data[0] & 0x3f;
    let mut stick_y = data[1] & 0x3f;
    if motion_plus_passthrough {
        stick_x &= 0x3e;
        stick_y &= 0x3e;
    }
    let stick = Axis3 {
        x: i32::from(stick_x) - 32,
        y: i32::from(stick_y) - 32,
        z: 0,
    };

    let which = (data[2] >> 1) & 0x1f;
    let no_pressure = data[2] & 0x40 != 0;
    let hi_hat_pressure = data[2] & 0x80 == 0;
    let pressure = i32::from(7 - (data[3] >> 5));
    let pad_pressure = |which_code, on_off| {
        if !no_pressure && which == which_code {
            pressure
        } else if on_off {
            3
        } else {
            0
        }
    };
    let pads = [
        pad_pressure(0x0e, data[5] & 0x80 == 0),
        pad_pressure(0x19, data[5] & 0x40 == 0),
        pad_pressure(0x11, data[5] & 0x20 == 0),
        pad_pressure(0x12, data[5] & 0x10 == 0),
        pad_pressure(0x0f, data[5] & 0x08 == 0),
        pad_pressure(
            if hi_hat_pressure { 0xff } else { 0x1b },
            data[5] & 0x04 == 0,
        ),
        if hi_hat_pressure && !no_pressure && which == 0x0e {
            pressure
        } else {
            0
        },
    ];
    let [orange, red, yellow, green, blue, bass, hi_hat] = pads;
    Some(ExtensionFrame {
        buttons,
        movement: EventKind::DrumsMove([
            stick,
            Axis3 {
                x: orange,
                y: 0,
                z: 0,
            },
            Axis3 { x: red, y: 0, z: 0 },
            Axis3 {
                x: yellow,
                y: 0,
                z: 0,
            },
            Axis3 {
                x: green,
                y: 0,
                z: 0,
            },
            Axis3 {
                x: blue,
                y: 0,
                z: 0,
            },
            Axis3 {
                x: bass,
                y: 0,
                z: 0,
            },
            Axis3 {
                x: hi_hat,
                y: 0,
                z: 0,
            },
        ]),
    })
}
fn decode_drums_passthrough(data: &[u8]) -> Option<ExtensionFrame> {
    decode_drums_report(data, true)
}

fn decode_accel(first_button: u8, second_button: u8, data: &[u8]) -> Axis3 {
    Axis3 {
        x: ((i32::from(data[0]) << 2) | i32::from((first_button >> 5) & 0x03)) - 0x200,
        y: ((i32::from(data[1]) << 2) | i32::from((second_button >> 4) & 0x02)) - 0x200,
        z: ((i32::from(data[2]) << 2) | i32::from((second_button >> 5) & 0x02)) - 0x200,
    }
}

fn decode_ir_basic(data: &[u8]) -> [Axis3; 4] {
    let point = |x_low: u8, y_low: u8, packed: u8, x_shift: u32, y_shift: u32| Axis3 {
        x: i32::from(x_low) | (i32::from((packed >> x_shift) & 0x03) << 8),
        y: i32::from(y_low) | (i32::from((packed >> y_shift) & 0x03) << 8),
        z: 0,
    };
    [
        point(data[0], data[1], data[2], 4, 6),
        point(data[3], data[4], data[2], 0, 2),
        point(data[5], data[6], data[7], 4, 6),
        point(data[8], data[9], data[7], 0, 2),
    ]
}

fn decode_ir_extended(data: &[u8]) -> [Axis3; 4] {
    std::array::from_fn(|index| {
        let offset = index * 3;
        let packed = data[offset + 2];
        Axis3 {
            x: i32::from(data[offset]) | (i32::from((packed >> 4) & 0x03) << 8),
            y: i32::from(data[offset + 1]) | (i32::from((packed >> 6) & 0x03) << 8),
            z: 0,
        }
    })
}

fn decode_button_bits(first: u8, second: u8) -> u16 {
    (u16::from(first & 0x1f)) | (u16::from(second & 0x1f) << 5) | (u16::from(second & 0x80) << 3)
}

pub(crate) fn button_event(
    kind: ExtensionKind,
    button: Button,
    state: ButtonState,
) -> Option<EventKind> {
    let event = ButtonEvent { button, state };
    Some(match kind {
        ExtensionKind::Nunchuk => EventKind::NunchukKey(event),
        ExtensionKind::Classic => EventKind::ClassicControllerKey(event),
        ExtensionKind::Pro => EventKind::ProControllerKey(event),
        ExtensionKind::Guitar => EventKind::GuitarKey(event),
        ExtensionKind::Drums => EventKind::DrumsKey(event),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AccelCalibration, BalanceCalibration, ExtensionKind, classify_extension_id,
        decode_balance_board, decode_data, is_motion_plus_id, memory_read_packet,
        memory_write_packet, parse_memory, report_modes,
    };
    use crate::decode::EventKind;
    use crate::model::{Axis3, InterfaceMask};

    #[test]
    fn report_37_decodes_ir_accel_and_extension_bytes_together() {
        let mut report = [0; 22];
        report[0] = 0x37;
        report[1] = 0;
        report[2] = 0x80;
        report[3..6].copy_from_slice(&[0x80, 0x80, 0x80]);
        report[6..16].copy_from_slice(&[10, 20, 0, 30, 40, 0, 50, 60, 0, 70]);
        report[16..22].copy_from_slice(&[128, 127, 0x80, 0x80, 0x80, 0x03]);
        let decoded = decode_data(&report, ExtensionKind::Nunchuk, false, None)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.accel, Some(Axis3 { x: 0, y: 0, z: 0 }));
        assert_eq!(decoded.ir.unwrap()[0], Axis3 { x: 10, y: 20, z: 0 });
        let Some(extension) = decoded.extension else {
            panic!("missing nunchuk frame");
        };
        assert_eq!(
            extension.movement,
            EventKind::NunchukMove([Axis3 { x: 0, y: -1, z: 0 }, Axis3 { x: 0, y: 0, z: 0 },])
        );
    }
    #[test]
    fn motion_plus_nunchuk_passthrough_restores_split_acceleration_bits() {
        let mut report = [0; 22];
        report[0] = 0x34;
        report[3..9].copy_from_slice(&[128, 127, 0x80, 0x80, 0x81, 0x0c]);
        let decoded = decode_data(&report, ExtensionKind::Nunchuk, true, None)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.motion_plus_extension_connected, Some(true));
        let Some(extension) = decoded.extension else {
            panic!("missing passthrough frame");
        };
        assert_eq!(
            extension.movement,
            EventKind::NunchukMove([Axis3 { x: 0, y: -1, z: 0 }, Axis3 { x: 0, y: 0, z: 0 },])
        );
    }

    #[test]
    fn classic_and_guitar_passthrough_reports_keep_their_sticks_centered() {
        let mut report = [0; 22];
        report[0] = 0x34;
        report[3..9].copy_from_slice(&[0xa1, 0x21, 0x10, 0, 0xff, 0xfc]);
        let classic = decode_data(&report, ExtensionKind::Classic, true, None)
            .unwrap()
            .unwrap();
        assert_eq!(classic.motion_plus_extension_connected, Some(true));
        assert_eq!(
            classic.extension.unwrap().movement,
            EventKind::ClassicControllerMove([
                Axis3 { x: 0, y: 0, z: 0 },
                Axis3 { x: 0, y: 0, z: 0 },
                Axis3 { x: 0, y: 0, z: 0 },
            ])
        );

        report[3..9].copy_from_slice(&[0x21, 0x21, 0, 0x10, 0xff, 0xfc]);
        let guitar = decode_data(&report, ExtensionKind::Guitar, true, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            guitar.extension.unwrap().movement,
            EventKind::GuitarMove([
                Axis3 { x: 0, y: 0, z: 0 },
                Axis3 { x: 0, y: 0, z: 0 },
                Axis3 { x: 0, y: 0, z: 0 },
            ])
        );
    }

    #[test]
    fn nunchuk_stick_endpoints_are_centered() {
        let mut report = [0; 11];
        report[0] = 0x32;
        report[3..9].copy_from_slice(&[0, 0xff, 0x80, 0x80, 0x80, 0x03]);
        let frame = decode_data(&report, ExtensionKind::Nunchuk, false, None)
            .unwrap()
            .unwrap();
        let EventKind::NunchukMove(movement) = frame.extension.unwrap().movement else {
            panic!("unexpected Nunchuk movement kind");
        };
        assert_eq!(
            movement[0],
            Axis3 {
                x: -128,
                y: 127,
                z: 0
            }
        );
    }

    #[test]
    fn classic_sticks_and_five_bit_triggers_reach_their_signed_ranges() {
        let mut report = [0; 11];
        report[0] = 0x32;
        report[3..9].copy_from_slice(&[0xe0, 0xe0, 0xff, 0xff, 0xff, 0xff]);
        let frame = decode_data(&report, ExtensionKind::Classic, false, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            frame.extension.unwrap().movement,
            EventKind::ClassicControllerMove([
                Axis3 { x: 0, y: 0, z: 0 },
                Axis3 { x: 30, y: 30, z: 0 },
                Axis3 { x: 62, y: 62, z: 0 },
            ])
        );

        report[3..9].copy_from_slice(&[0, 0, 0, 0, 0xff, 0xff]);
        let frame = decode_data(&report, ExtensionKind::Classic, false, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            frame.extension.unwrap().movement,
            EventKind::ClassicControllerMove([
                Axis3 {
                    x: -32,
                    y: -32,
                    z: 0
                },
                Axis3 {
                    x: -32,
                    y: -32,
                    z: 0
                },
                Axis3 { x: 0, y: 0, z: 0 },
            ])
        );
    }

    #[test]
    fn guitar_sticks_and_whammy_are_centered() {
        let mut report = [0; 11];
        report[0] = 0x32;
        report[3..9].copy_from_slice(&[0, 0x3f, 0x1f, 0, 0xff, 0xff]);
        let frame = decode_data(&report, ExtensionKind::Guitar, false, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            frame.extension.unwrap().movement,
            EventKind::GuitarMove([
                Axis3 {
                    x: -32,
                    y: 31,
                    z: 0
                },
                Axis3 { x: -16, y: 0, z: 0 },
                Axis3 { x: 31, y: 0, z: 0 },
            ])
        );
    }

    #[test]
    fn pro_report_uses_eleven_byte_layout_for_axes_and_buttons() {
        assert_eq!(
            classify_extension_id(&[0x01, 0, 0xa4, 0x20, 0x01, 0x20]),
            ExtensionKind::Pro
        );
        assert_eq!(
            classify_extension_id(&[0x01, 0, 0xa4, 0x20, 0x01, 0x01]),
            ExtensionKind::Unknown
        );
        let short = [0x32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(
            decode_data(&short, ExtensionKind::Pro, false, None)
                .unwrap()
                .unwrap()
                .extension
                .is_none()
        );

        let mut report = [0; 22];
        report[0] = 0x34;
        report[3..14].copy_from_slice(&[0, 0x08, 0, 0x08, 0, 0x08, 0, 0x08, 0xff, 0xff, 0xff]);
        let neutral = decode_data(&report, ExtensionKind::Pro, false, None)
            .unwrap()
            .unwrap()
            .extension
            .unwrap();
        assert_eq!(neutral.buttons, 0);
        assert_eq!(
            neutral.movement,
            EventKind::ProControllerMove([Axis3 { x: 0, y: 0, z: 0 }, Axis3 { x: 0, y: 0, z: 0 },])
        );
        report[3..14].copy_from_slice(&[0, 0, 0xff, 0x0f, 0xff, 0x0f, 0, 0, 0xff, 0xff, 0xff]);
        let frame = decode_data(&report, ExtensionKind::Pro, false, None)
            .unwrap()
            .unwrap()
            .extension
            .unwrap();
        assert_eq!(frame.buttons, 0);
        assert_eq!(
            frame.movement,
            EventKind::ProControllerMove([
                Axis3 {
                    x: -2048,
                    y: -2047,
                    z: 0,
                },
                Axis3 {
                    x: 2047,
                    y: 2048,
                    z: 0,
                },
            ])
        );

        report[12] = 0xef;
        report[13] = 0xfd;
        let frame = decode_data(&report, ExtensionKind::Pro, false, None)
            .unwrap()
            .unwrap()
            .extension
            .unwrap();
        assert_eq!(frame.buttons, 1 | (1 << 15));
    }

    #[test]
    fn motion_plus_center_axes_order_and_rate_scaling_are_preserved() {
        let mut report = [0; 22];
        report[0] = 0x34;
        report[3..9].copy_from_slice(&[0, 0, 0, 0x80, 0x80, 0x82]);
        let frame = decode_data(&report, ExtensionKind::None, true, None)
            .unwrap()
            .unwrap();
        assert_eq!(frame.motion_plus_extension_connected, Some(false));
        assert_eq!(
            frame.extension.unwrap().movement,
            EventKind::MotionPlus(Axis3 { x: 0, y: 0, z: 0 })
        );

        report[3..9].copy_from_slice(&[1, 0xff, 2, 0x83, 0x7e, 0x82]);
        let movement = decode_data(&report, ExtensionKind::None, true, None)
            .unwrap()
            .unwrap()
            .extension
            .unwrap()
            .movement;
        assert_eq!(
            movement,
            EventKind::MotionPlus(Axis3 { x: 9, y: -9, z: 18 })
        );

        report[3..9].copy_from_slice(&[1, 0, 0, 0x80, 0x80, 0x82]);
        let movement = decode_data(&report, ExtensionKind::None, true, None)
            .unwrap()
            .unwrap()
            .extension
            .unwrap()
            .movement;
        assert_eq!(movement, EventKind::MotionPlus(Axis3 { x: 40, y: 0, z: 0 }));

        report[3..9].copy_from_slice(&[0, 0, 0, 0x03, 0x80, 0x82]);
        let movement = decode_data(&report, ExtensionKind::None, true, None)
            .unwrap()
            .unwrap()
            .extension
            .unwrap()
            .movement;
        assert_eq!(
            movement,
            EventKind::MotionPlus(Axis3 {
                x: -73_728,
                y: 0,
                z: 0,
            })
        );
        report[3..9].copy_from_slice(&[0xff, 0, 0, 0xff, 0x80, 0x82]);
        let movement = decode_data(&report, ExtensionKind::None, true, None)
            .unwrap()
            .unwrap()
            .extension
            .unwrap()
            .movement;
        assert_eq!(
            movement,
            EventKind::MotionPlus(Axis3 {
                x: 73_719,
                y: 0,
                z: 0,
            })
        );
    }

    #[test]
    fn invalid_motion_plus_transition_packets_produce_no_extension_events() {
        let mut report = [0; 22];
        report[0] = 0x34;
        for invalid_payload in [[0, 0, 0, 0x80, 0x80, 0x81], [0, 0, 0, 0x80, 0x80, 0x83]] {
            report[3..9].copy_from_slice(&invalid_payload);
            let frame = decode_data(&report, ExtensionKind::Nunchuk, true, None)
                .unwrap()
                .unwrap();
            assert!(frame.extension.is_none());
            assert_eq!(frame.motion_plus_extension_connected, None);
        }
    }

    #[test]
    fn balance_board_calibrates_each_corner_at_zero_seventeen_and_thirty_four_kg() {
        let corner_points: [[u16; 3]; 4] = [
            [100, 200, 300],
            [1000, 2000, 3000],
            [10, 20, 30],
            [200, 400, 600],
        ];
        let mut calibration_bytes = [0; 24];
        for (corner, points) in corner_points.iter().enumerate() {
            for (point, value) in points.iter().enumerate() {
                let offset = (point * 4 + corner) * 2;
                calibration_bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
            }
        }
        let calibration = BalanceCalibration::from_bytes(calibration_bytes);
        let board_bytes = |samples: [u16; 4]| {
            let mut bytes = [0; 8];
            for (corner, sample) in samples.into_iter().enumerate() {
                bytes[corner * 2..corner * 2 + 2].copy_from_slice(&sample.to_be_bytes());
            }
            bytes
        };
        assert_eq!(
            decode_balance_board(&board_bytes([100, 1000, 10, 200]), &calibration).unwrap(),
            [Axis3::default(); 4]
        );
        assert_eq!(
            decode_balance_board(&board_bytes([200, 2000, 20, 400]), &calibration).unwrap(),
            [Axis3 {
                x: 1700,
                y: 0,
                z: 0
            }; 4]
        );
        assert_eq!(
            decode_balance_board(&board_bytes([300, 3000, 30, 600]), &calibration).unwrap(),
            [Axis3 {
                x: 3400,
                y: 0,
                z: 0
            }; 4]
        );
        let raw = board_bytes([150, 2500, 30, 600]);
        assert_eq!(
            decode_balance_board(&raw, &calibration).unwrap(),
            [
                Axis3 { x: 850, y: 0, z: 0 },
                Axis3 {
                    x: 2550,
                    y: 0,
                    z: 0
                },
                Axis3 {
                    x: 3400,
                    y: 0,
                    z: 0
                },
                Axis3 {
                    x: 3400,
                    y: 0,
                    z: 0
                },
            ]
        );
        assert!(decode_balance_board(&raw[..7], &calibration).is_none());

        let mut report = [0; 11];
        report[0] = 0x32;
        report[3..11].copy_from_slice(&raw);
        assert!(
            decode_data(&report, ExtensionKind::BalanceBoard, false, None)
                .unwrap()
                .unwrap()
                .extension
                .is_none()
        );
        let frame = decode_data(
            &report,
            ExtensionKind::BalanceBoard,
            false,
            Some(&calibration),
        )
        .unwrap()
        .unwrap()
        .extension
        .unwrap();
        assert_eq!(
            frame.movement,
            EventKind::BalanceBoard(decode_balance_board(&raw, &calibration).unwrap())
        );
    }

    #[test]
    fn drums_report_each_pad_pressure_and_active_low_plus_minus() {
        let mut report = [0; 11];
        report[0] = 0x32;
        report[3..9].copy_from_slice(&[32, 32, 0xb2, 0x60, 0x14, 0xfc]);
        let decoded = decode_data(&report, ExtensionKind::Drums, false, None)
            .unwrap()
            .unwrap();
        let Some(extension) = decoded.extension else {
            panic!("missing drum frame");
        };
        assert_eq!(extension.buttons, 0);
        assert_eq!(
            extension.movement,
            EventKind::DrumsMove([
                Axis3 { x: 0, y: 0, z: 0 },
                Axis3 { x: 0, y: 0, z: 0 },
                Axis3 { x: 4, y: 0, z: 0 },
                Axis3 { x: 0, y: 0, z: 0 },
                Axis3 { x: 0, y: 0, z: 0 },
                Axis3 { x: 0, y: 0, z: 0 },
                Axis3 { x: 0, y: 0, z: 0 },
                Axis3 { x: 0, y: 0, z: 0 },
            ])
        );

        for (which, hi_hat, slot) in [
            (0x0e, false, 1),
            (0x19, false, 2),
            (0x11, false, 3),
            (0x12, false, 4),
            (0x0f, false, 5),
            (0x1b, false, 6),
            (0x0e, true, 7),
        ] {
            report[5] = ((which << 1) | (if hi_hat { 0 } else { 0x80 })) as u8;
            report[6] = 0;
            let frame = decode_data(&report, ExtensionKind::Drums, false, None)
                .unwrap()
                .unwrap()
                .extension
                .unwrap();
            let EventKind::DrumsMove(values) = frame.movement else {
                panic!("unexpected drum movement kind");
            };
            assert_eq!(values[slot].x, 7);
        }

        report[5] = 0xb2;
        report[6] = 0xe0;
        let minimum_pressure = decode_data(&report, ExtensionKind::Drums, false, None)
            .unwrap()
            .unwrap()
            .extension
            .unwrap();
        let EventKind::DrumsMove(values) = minimum_pressure.movement else {
            panic!("unexpected drum movement kind");
        };
        assert_eq!(values[2].x, 0);

        report[7] = 0x10;
        let plus_pressed = decode_data(&report, ExtensionKind::Drums, false, None)
            .unwrap()
            .unwrap()
            .extension
            .unwrap();
        assert_eq!(plus_pressed.buttons, 1);
        report[7] = 0x04;
        let minus_pressed = decode_data(&report, ExtensionKind::Drums, false, None)
            .unwrap()
            .unwrap()
            .extension
            .unwrap();
        assert_eq!(minus_pressed.buttons, 1 << 1);
        report[7] = 0x14;
        report[5] = 0x72;
        report[8] = 0xbc;
        let pressed = decode_data(&report, ExtensionKind::Drums, false, None)
            .unwrap()
            .unwrap()
            .extension
            .unwrap();
        let EventKind::DrumsMove(values) = pressed.movement else {
            panic!("unexpected drum movement kind");
        };
        assert_eq!(values[2].x, 3);

        report[8] = 0xfc;
        let idle = decode_data(&report, ExtensionKind::Drums, false, None)
            .unwrap()
            .unwrap()
            .extension
            .unwrap();
        assert_eq!(idle.buttons, 0);
        let EventKind::DrumsMove(values) = idle.movement else {
            panic!("unexpected drum movement kind");
        };
        assert!(values[1..].iter().all(|value| value.x == 0));
    }

    #[test]
    fn ir_reports_use_all_coordinate_bits_for_sentinels() {
        let mut extended = [0; 22];
        extended[0] = 0x33;
        extended[6..18].copy_from_slice(&[0xff, 0xff, 0x0f, 1, 2, 0, 3, 4, 0, 0xff, 0xff, 0xff]);
        let points = decode_data(&extended, ExtensionKind::None, false, None)
            .unwrap()
            .unwrap()
            .ir
            .unwrap();
        assert_eq!(
            points[0],
            Axis3 {
                x: 255,
                y: 255,
                z: 0
            }
        );
        assert_eq!(points[1], Axis3 { x: 1, y: 2, z: 0 });
        assert_eq!(points[2], Axis3 { x: 3, y: 4, z: 0 });
        assert_eq!(
            points[3],
            Axis3 {
                x: 1023,
                y: 1023,
                z: 0
            }
        );

        let mut basic = [0; 22];
        basic[0] = 0x36;
        basic[3..13].copy_from_slice(&[0xff, 0xff, 0x0f, 0xff, 0xff, 0x00, 0, 0, 0, 0]);
        let points = decode_data(&basic, ExtensionKind::None, false, None)
            .unwrap()
            .unwrap()
            .ir
            .unwrap();
        assert_eq!(
            points[0],
            Axis3 {
                x: 255,
                y: 255,
                z: 0
            }
        );
        assert_eq!(
            points[1],
            Axis3 {
                x: 1023,
                y: 1023,
                z: 0
            }
        );
    }

    #[test]
    fn extension_identity_and_report_plan_cover_long_sensor_payloads() {
        assert_eq!(
            classify_extension_id(&[0, 0, 0xa4, 0x20, 4, 2]),
            ExtensionKind::BalanceBoard
        );
        assert_eq!(
            classify_extension_id(&[0, 0, 0xa4, 0x20, 1, 1]),
            ExtensionKind::Classic
        );
        assert!(is_motion_plus_id(&[0, 0, 0xa6, 0x20, 0, 5]));
        let plan = report_modes(
            InterfaceMask::ACCEL | InterfaceMask::IR | InterfaceMask::BALANCE_BOARD,
            ExtensionKind::BalanceBoard,
        )
        .unwrap();
        assert!(plan.contains(0x33));
        assert!(plan.contains(0x35));
    }

    #[test]
    fn memory_read_and_chunked_response_preserve_big_endian_offsets() {
        assert_eq!(
            memory_read_packet(0xa400fa, 6, true, true),
            [0x17, 0x05, 0xa4, 0, 0xfa, 0, 6]
        );
        let mut report = [0; 22];
        report[0] = 0x21;
        report[3] = 0x90;
        report[4..6].copy_from_slice(&[0x00, 0x16]);
        report[6..22]
            .copy_from_slice(&[0x80, 0x80, 0x80, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let memory = parse_memory(&report).unwrap();
        assert_eq!(memory.offset, 0x16);
        assert_eq!(memory.len, 10);
        assert_eq!(memory.error, 0);
        let packet = memory_write_packet(0xa400f0, &[0x55], true, false).unwrap();
        assert_eq!(&packet[..7], &[0x16, 0x06, 0xa4, 0x00, 0xf0, 1, 0x55]);
    }

    #[test]
    fn accelerometer_calibration_requires_valid_checksum() {
        let mut bytes = [0x80, 0x80, 0x80, 0x00, 0x90, 0x90, 0x90, 0x00, 0x40, 0];
        bytes[9] = bytes[..9]
            .iter()
            .fold(0x55u8, |sum, byte| sum.wrapping_add(*byte));
        assert_eq!(
            AccelCalibration::from_eeprom(&bytes).unwrap().zero,
            Axis3 {
                x: 512,
                y: 512,
                z: 512
            }
        );
        bytes[9] ^= 1;
        assert_eq!(AccelCalibration::from_eeprom(&bytes), None);
    }
}
