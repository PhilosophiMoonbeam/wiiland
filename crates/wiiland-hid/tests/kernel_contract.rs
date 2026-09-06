use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::Path;

use wiiland_hid::{
    Axis3, Button, ButtonEvent, ButtonState, Event, EventKind, EventType, Interface, InterfaceMask,
    OpenError, Timestamp,
};

#[test]
fn public_interface_owns_kernel_resources_and_uses_io_errors() {
    let constructor: fn(&Path) -> io::Result<Interface> = Interface::new;
    let _ = constructor;

    fn interface_fd<'a>(interface: &'a Interface) -> BorrowedFd<'a> {
        interface.as_fd()
    }
    let _ = interface_fd as fn(&Interface) -> BorrowedFd<'_>;

    fn open_error_is_typed<E: std::error::Error>() {}
    open_error_is_typed::<OpenError>();
}

#[test]
fn facade_exposes_owned_timestamp_buttons_and_event_discriminants() {
    let timestamp = Timestamp {
        seconds: -4,
        microseconds: 123_456,
    };
    assert_eq!(timestamp.seconds, -4);
    assert_eq!(timestamp.microseconds, 123_456);

    let buttons = [
        Button::Left,
        Button::Right,
        Button::Up,
        Button::Down,
        Button::Plus,
        Button::Minus,
        Button::One,
        Button::Two,
        Button::A,
        Button::B,
        Button::Home,
        Button::C,
        Button::Z,
        Button::X,
        Button::Y,
        Button::ShoulderLeft,
        Button::ShoulderRight,
        Button::TriggerLeft,
        Button::TriggerRight,
        Button::ThumbLeft,
        Button::ThumbRight,
        Button::StrumBarUp,
        Button::StrumBarDown,
        Button::FretFarUp,
        Button::FretUp,
        Button::FretMid,
        Button::FretLow,
        Button::FretFarLow,
    ];
    assert_eq!(buttons.len(), 28);
    assert_eq!(
        [
            ButtonState::Released,
            ButtonState::Pressed,
            ButtonState::Repeated
        ]
        .len(),
        3
    );

    let event = Event {
        time: timestamp,
        kind: EventKind::Key(ButtonEvent {
            button: Button::A,
            state: ButtonState::Pressed,
        }),
    };
    assert_eq!(event.time, timestamp);
    assert_eq!(
        event.kind,
        EventKind::Key(ButtonEvent {
            button: Button::A,
            state: ButtonState::Pressed,
        })
    );

    let event_types = [
        EventType::Key,
        EventType::Accel,
        EventType::Ir,
        EventType::BalanceBoard,
        EventType::MotionPlus,
        EventType::ProControllerKey,
        EventType::ProControllerMove,
        EventType::Watch,
        EventType::ClassicControllerKey,
        EventType::ClassicControllerMove,
        EventType::NunchukKey,
        EventType::NunchukMove,
        EventType::DrumsKey,
        EventType::DrumsMove,
        EventType::GuitarKey,
        EventType::GuitarMove,
        EventType::Gone,
    ];
    assert_eq!(event_types.len(), 17);
}

#[test]
fn interface_masks_preserve_named_interface_sets() {
    let masks = [
        InterfaceMask::CORE,
        InterfaceMask::ACCEL,
        InterfaceMask::IR,
        InterfaceMask::MOTION_PLUS,
        InterfaceMask::NUNCHUK,
        InterfaceMask::CLASSIC_CONTROLLER,
        InterfaceMask::BALANCE_BOARD,
        InterfaceMask::PRO_CONTROLLER,
        InterfaceMask::DRUMS,
        InterfaceMask::GUITAR,
    ];
    let mut all = InterfaceMask::empty();
    for mask in masks {
        assert!(!all.contains(mask));
        all = all.insert(mask);
        assert!(all.contains(mask));
    }
    assert_eq!(all, InterfaceMask::ALL);
    assert!(all.contains(InterfaceMask::ALL));
    assert!((all | InterfaceMask::WRITABLE).contains(InterfaceMask::WRITABLE));
    assert_eq!(all & InterfaceMask::ACCEL, InterfaceMask::ACCEL);
    assert!(!all.remove(InterfaceMask::IR).contains(InterfaceMask::IR));
    assert_eq!(
        !InterfaceMask::empty() & InterfaceMask::ALL,
        InterfaceMask::ALL
    );
}

#[test]
fn axis_values_remain_owned_and_three_dimensional() {
    let axis = Axis3 { x: -1, y: 2, z: 3 };
    assert_eq!((axis.x, axis.y, axis.z), (-1, 2, 3));
}

#[test]
fn logical_button_codes_round_trip_in_the_public_facade() {
    for (code, button) in wiiland_hid::Button::ALL.into_iter().enumerate() {
        assert_eq!(button.code(), code as u32);
        assert_eq!(wiiland_hid::Button::from_code(code as u32), Some(button));
    }
    assert_eq!(wiiland_hid::Button::from_code(28), None);
    assert_eq!(wiiland_hid::Button::from_code(u32::MAX), None);
}
