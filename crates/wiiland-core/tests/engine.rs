use wiiland_core::engine::{DeviceEngine, EngineInput, OutputAction as O, OutputDevice as D};
use wiiland_core::pointer::{IrFrame, IrPoint};
use wiiland_core::{Config, Profile, ValidatedConfig};

fn engine(profile: Profile) -> DeviceEngine {
    DeviceEngine::new(Config::default().try_into().unwrap(), profile)
}
fn key(code: u32, state: u32) -> EngineInput {
    EngineInput::Key { code, state }
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
    for state in 0..=2 {
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
