//! Logical controller inputs shared by hardware adapters and the deterministic engine.

/// A logical controller button.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum Button {
    Left,
    Right,
    Up,
    Down,
    Plus,
    Minus,
    One,
    Two,
    A,
    B,
    Home,
    C,
    Z,
    X,
    Y,
    ShoulderLeft,
    ShoulderRight,
    TriggerLeft,
    TriggerRight,
    ThumbLeft,
    ThumbRight,
    StrumBarUp,
    StrumBarDown,
    FretFarUp,
    FretUp,
    FretMid,
    FretLow,
    FretFarLow,
}

impl Button {
    pub const ALL: [Self; 28] = [
        Button::Left,
        Button::Right,
        Button::Up,
        Button::Down,
        Button::A,
        Button::B,
        Button::Plus,
        Button::Minus,
        Button::Home,
        Button::One,
        Button::Two,
        Button::X,
        Button::Y,
        Button::ShoulderLeft,
        Button::ShoulderRight,
        Button::TriggerLeft,
        Button::TriggerRight,
        Button::ThumbLeft,
        Button::ThumbRight,
        Button::C,
        Button::Z,
        Button::StrumBarUp,
        Button::StrumBarDown,
        Button::FretFarUp,
        Button::FretUp,
        Button::FretMid,
        Button::FretLow,
        Button::FretFarLow,
    ];
    pub fn from_code(code: u32) -> Option<Self> {
        Self::ALL.get(code as usize).copied()
    }

    /// Stable logical button number used by mapping and diagnostic contracts.
    pub const fn code(self) -> u32 {
        match self {
            Button::Left => 0,
            Button::Right => 1,
            Button::Up => 2,
            Button::Down => 3,
            Button::Plus => 6,
            Button::Minus => 7,
            Button::One => 9,
            Button::Two => 10,
            Button::A => 4,
            Button::B => 5,
            Button::Home => 8,
            Button::C => 19,
            Button::Z => 20,
            Button::X => 11,
            Button::Y => 12,
            Button::ShoulderLeft => 13,
            Button::ShoulderRight => 14,
            Button::TriggerLeft => 15,
            Button::TriggerRight => 16,
            Button::ThumbLeft => 17,
            Button::ThumbRight => 18,
            Button::StrumBarUp => 21,
            Button::StrumBarDown => 22,
            Button::FretFarUp => 23,
            Button::FretUp => 24,
            Button::FretMid => 25,
            Button::FretLow => 26,
            Button::FretFarLow => 27,
        }
    }
}

impl ButtonState {
    /// Linux-compatible released, pressed, or repeated value.
    pub const fn value(self) -> u32 {
        match self {
            Self::Released => 0,
            Self::Pressed => 1,
            Self::Repeated => 2,
        }
    }
}

/// The state of a logical controller button.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ButtonState {
    Released,
    Pressed,
    Repeated,
}

/// A decoded button transition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ButtonEvent {
    pub button: Button,
    pub state: ButtonState,
}

/// The independently removable interface that owns a button transition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InputSource {
    Core,
    Nunchuk,
    ClassicController,
    ProController,
    Drums,
    Guitar,
}
impl InputSource {
    pub const ALL: [Self; 6] = [
        Self::Core,
        Self::Nunchuk,
        Self::ClassicController,
        Self::ProController,
        Self::Drums,
        Self::Guitar,
    ];
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Core => 0,
            Self::Nunchuk => 1,
            Self::ClassicController => 2,
            Self::ProController => 3,
            Self::Drums => 4,
            Self::Guitar => 5,
        }
    }
}
