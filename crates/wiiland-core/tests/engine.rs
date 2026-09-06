use wiiland_core::engine::{DeviceEngine, EngineInput, OutputAction as O, OutputDevice as D};
use wiiland_core::input::{Button, ButtonState, InputSource};
use wiiland_core::mapping::{Abs3, MotionKind};
use wiiland_core::pointer::{IrFrame, IrPoint};
use wiiland_core::{Config, Profile, ValidatedConfig};

fn engine(profile: Profile) -> DeviceEngine {
    DeviceEngine::new(Config::default().try_into().unwrap(), profile)
}
fn key(code: u32, state: u32) -> EngineInput {
    EngineInput::Key {
        source: InputSource::Core,
        button: Button::from_code(code).unwrap(),
        state: match state {
            0 => ButtonState::Released,
            1 => ButtonState::Pressed,
            2 => ButtonState::Repeated,
            _ => panic!("invalid state"),
        },
    }
}

#[test]
fn held_pointer_session_reset_and_reconnection_do_not_keep_moving() {
    let mut engine = engine(Profile::DESKTOP);
    let mut out = Vec::new();
    engine.process(key(1, 1), &mut out);
    assert_eq!(
        out,
        [
            O::Rel(D::Desktop, 0, 16),
            O::Rel(D::Desktop, 1, 0),
            O::Sync(D::Desktop)
        ]
    );
    out.clear();
    engine.process(EngineInput::PointerTick, &mut out);
    assert_eq!(out.len(), 3);
    engine.process(EngineInput::Reset, &mut out);
    assert!(!engine.pointer_active());
    out.clear();
    engine.process(EngineInput::PointerTick, &mut out);
    assert!(out.is_empty());
    engine.process(key(0, 1), &mut out);
    assert_eq!(out[0], O::Rel(D::Desktop, 0, -16));
    out.clear();
    engine.process(key(0, 0), &mut out);
    assert!(!engine.pointer_active());
    assert!(
        !out.iter()
            .any(|event| matches!(event, O::Rel(_, _, n) if *n != 0))
    );
}

#[test]
fn combined_profile_preserves_key_press_repeat_release_in_both_outputs() {
    let mut engine = engine(Profile::BOTH);
    for state in [1, 2, 0] {
        let mut out = Vec::new();
        engine.process(key(4, state), &mut out);
        assert_eq!(
            out,
            [
                O::Key(D::Gamepad, 0x130, state),
                O::Key(D::Desktop, 0x110, state)
            ]
        );
    }
}

#[test]
fn ir_loss_and_reacquisition_establish_a_new_baseline() {
    let mut engine = engine(Profile::DESKTOP);
    let frame = |x| {
        EngineInput::Ir(IrFrame {
            points: [IrPoint {
                valid: true,
                x,
                y: 100,
            }; 4],
        })
    };
    let mut out = Vec::new();
    engine.process(frame(100), &mut out);
    out.clear();
    engine.process(frame(164), &mut out);
    assert!(out.contains(&O::Rel(D::Desktop, 0, 8)));
    engine.process(EngineInput::Ir(IrFrame::default()), &mut out);
    out.clear();
    engine.process(frame(900), &mut out);
    assert!(
        !out.iter()
            .any(|event| matches!(event, O::Rel(_, _, n) if *n != 0))
    );
}

#[test]
fn invalid_drafts_cannot_become_runtime_snapshots() {
    let draft = Config {
        pointer_speed: 0,
        ..Config::default()
    };
    assert!(ValidatedConfig::try_from(draft).is_err());
}

fn sourced_key(source: InputSource, button: Button, state: ButtonState) -> EngineInput {
    EngineInput::Key {
        source,
        button,
        state,
    }
}

fn motion(x: i32) -> EngineInput {
    let mut axes = [Abs3 { x: 0, y: 0, z: 0 }; 8];
    axes[0].x = x;
    EngineInput::Motion {
        kind: MotionKind::MotionPlus,
        axes,
    }
}

#[test]
fn session_reset_clears_activation_until_a_new_press() {
    use wiiland_core::{AimActivation, AimMode, AimSource};
    let config = Config {
        aim_mode: AimMode::Mouse,
        aim_source: AimSource::MotionPlus,
        aim_activation: AimActivation::Z,
        aim_deadzone: 0,
        aim_smoothing: 0,
        ..Config::default()
    };
    let mut engine = DeviceEngine::new(config.try_into().unwrap(), Profile::GAMEPAD);
    let mut out = Vec::new();
    let press = sourced_key(InputSource::Nunchuk, Button::Z, ButtonState::Pressed);
    engine.process(press, &mut out);
    engine.process(motion(100), &mut out);
    assert!(out.contains(&O::Rel(D::Desktop, 0, 100)));
    engine.process(EngineInput::Reset, &mut out);
    out.clear();
    engine.process(motion(100), &mut out);
    assert!(
        !out.iter()
            .any(|action| matches!(action, O::Rel(_, _, value) if *value != 0))
    );
    out.clear();
    engine.process(press, &mut out);
    engine.process(motion(100), &mut out);
    assert!(out.contains(&O::Rel(D::Desktop, 0, 100)));
}

#[test]
fn overlapping_desktop_bindings_release_only_after_the_last_owner() {
    let mut config = Config::default();
    config.desktop_bindings.b = config.desktop_bindings.a;
    let mut engine = DeviceEngine::new(config.try_into().unwrap(), Profile::DESKTOP);
    let mut out = Vec::new();
    engine.process(key(4, 1), &mut out);
    engine.process(key(5, 1), &mut out);
    engine.process(key(4, 0), &mut out);
    assert_eq!(out, [O::Key(D::Desktop, 0x110, 1)]);
    engine.process(key(5, 2), &mut out);
    engine.process(key(5, 0), &mut out);
    assert_eq!(
        out,
        [
            O::Key(D::Desktop, 0x110, 1),
            O::Key(D::Desktop, 0x110, 2),
            O::Key(D::Desktop, 0x110, 0)
        ]
    );
}

#[test]
fn removal_releases_only_the_last_interface_owning_a_gamepad_button() {
    let mut engine = engine(Profile::GAMEPAD);
    let mut out = Vec::new();
    engine.process(
        sourced_key(InputSource::Core, Button::A, ButtonState::Pressed),
        &mut out,
    );
    engine.process(
        sourced_key(
            InputSource::ClassicController,
            Button::A,
            ButtonState::Pressed,
        ),
        &mut out,
    );
    engine.process(
        EngineInput::SourceRemoved(InputSource::ClassicController),
        &mut out,
    );
    assert_eq!(out, [O::Key(D::Gamepad, 0x130, 1)]);
    engine.process(EngineInput::SourceRemoved(InputSource::Core), &mut out);
    assert_eq!(
        out,
        [O::Key(D::Gamepad, 0x130, 1), O::Key(D::Gamepad, 0x130, 0)]
    );
    out.clear();
    engine.process(EngineInput::SourceRemoved(InputSource::Core), &mut out);
    assert!(out.is_empty(), "removal is idempotent");
}

#[test]
fn pointer_direction_stays_held_until_all_sources_release() {
    let mut engine = engine(Profile::DESKTOP);
    let mut out = Vec::new();
    engine.process(
        sourced_key(InputSource::Core, Button::Right, ButtonState::Pressed),
        &mut out,
    );
    engine.process(
        sourced_key(
            InputSource::ClassicController,
            Button::Right,
            ButtonState::Pressed,
        ),
        &mut out,
    );
    engine.process(EngineInput::SourceRemoved(InputSource::Core), &mut out);
    assert!(engine.pointer_active());
    out.clear();
    engine.process(EngineInput::PointerTick, &mut out);
    assert!(out.contains(&O::Rel(D::Desktop, 0, 16)));
    engine.process(
        EngineInput::SourceRemoved(InputSource::ClassicController),
        &mut out,
    );
    assert!(!engine.pointer_active());
}

#[test]
fn removing_activation_source_stops_aim_but_tracking_loss_retains_activation() {
    use wiiland_core::{AimActivation, AimMode, AimSource};
    let config = Config {
        aim_mode: AimMode::Mouse,
        aim_source: AimSource::Auto,
        aim_activation: AimActivation::Z,
        aim_deadzone: 0,
        aim_smoothing: 0,
        ..Config::default()
    };
    let mut engine = DeviceEngine::new(config.try_into().unwrap(), Profile::GAMEPAD);
    let mut out = Vec::new();
    engine.process(
        sourced_key(InputSource::Nunchuk, Button::Z, ButtonState::Pressed),
        &mut out,
    );
    engine.process(
        EngineInput::Ir(IrFrame {
            points: [IrPoint {
                valid: true,
                x: 100,
                y: 100,
            }; 4],
        }),
        &mut out,
    );
    engine.process(EngineInput::Ir(IrFrame::default()), &mut out);
    out.clear();
    engine.process(motion(100), &mut out);
    assert!(
        out.contains(&O::Rel(D::Desktop, 0, 100)),
        "tracking loss preserves held activation"
    );
    engine.process(EngineInput::SourceRemoved(InputSource::Nunchuk), &mut out);
    out.clear();
    engine.process(motion(100), &mut out);
    assert!(
        !out.iter()
            .any(|action| matches!(action, O::Rel(_, _, value) if *value != 0))
    );
}
