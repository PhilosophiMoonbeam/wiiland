use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use wiiland_core::{
    AimActivation, AimMode, AimSource, Backend, Config, ConfigError, DesktopAction, DeviceRuleKind,
    IrAimMapping, IrTracking, MAX_DEVICE_RULES, MAX_LINE_BYTES, Profile, SensorCalibration,
};

static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

fn temp_path(name: &str) -> PathBuf {
    let n = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("wiiland-core-{name}-{}-{n}", std::process::id()))
}

fn write_temp(name: &str, content: &[u8]) -> PathBuf {
    let path = temp_path(name);
    fs::write(&path, content).expect("write temporary configuration");
    path
}

fn remove(path: &Path) {
    let _ = fs::remove_file(path);
}

fn err_line(result: Result<(), ConfigError>, line: usize, fragment: &str) {
    let error = result.expect_err("configuration line should be rejected");
    assert_eq!(error.line, Some(line));
    assert!(
        error.message.contains(fragment),
        "{} does not contain {fragment:?}",
        error.message
    );
}

#[test]
fn defaults_and_all_scalar_lines_match_contract() {
    let mut config = Config::default();
    assert_eq!(config.backend, Backend::Uinput);
    assert_eq!(config.profile, Profile::GAMEPAD);
    assert_eq!(config.pointer_speed, 16);
    assert_eq!(config.ir_speed, 8);
    assert_eq!(config.ir_deadzone, 0);
    assert_eq!(config.ir_smoothing, 0);
    assert_eq!(config.ir_tracking, IrTracking::Dual);
    assert_eq!(config.ir_aim_mapping, IrAimMapping::Relative);
    assert_eq!(config.aim_mode, AimMode::Off);
    assert_eq!(config.aim_source, AimSource::Auto);
    assert_eq!(config.aim_activation, AimActivation::B);
    assert_eq!(config.aim_sensitivity, 16);
    assert_eq!(config.aim_deadzone, 4);
    assert_eq!(config.aim_smoothing, 25);
    assert!(!config.aim_invert_x && !config.aim_invert_y);
    assert!(config.ir_screen.is_none());
    assert!(config.aim_accel_zero.is_none() && config.aim_motion_plus_bias.is_none());

    let lines = [
        " profile = desktop # comment\n",
        "backend=uinput",
        "pointer-speed=31",
        "ir-speed=16",
        "ir-deadzone=4",
        "ir-smoothing=25",
        "ir-tracking=centroid",
        "ir-aim-mapping=absolute",
        "ir-screen-left=12",
        "ir-screen-right=900",
        "ir-screen-top=34",
        "ir-screen-bottom=700",
        "aim-mode=right-stick",
        "aim-source=motion-plus",
        "aim-activation=z",
        "aim-sensitivity=24",
        "aim-deadzone=12",
        "aim-smoothing=40",
        "aim-invert-x=yes",
        "aim-invert-y=1",
        "aim-accel-zero-x=-12",
        "aim-accel-zero-y=34",
        "aim-accel-zero-z=56",
        "aim-motion-plus-bias-x=-3",
        "aim-motion-plus-bias-y=9",
        "aim-motion-plus-bias-z=7",
        "aim-calibration-duration=12",
    ];
    for (line, text) in lines.into_iter().enumerate() {
        config.apply_line("self-test", line + 1, text).unwrap();
    }
    config
        .apply_line("self-test", 30, "desktop.a=enter")
        .unwrap();
    config
        .apply_line("self-test", 31, "desktop.b=disabled")
        .unwrap();
    assert_eq!(config.profile, Profile::DESKTOP);
    assert_eq!(config.ir_screen.unwrap().left, 12);
    assert_eq!(config.desktop_bindings.a, DesktopAction::Enter);
    assert_eq!(config.desktop_bindings.b, DesktopAction::Disabled);
    assert_eq!(
        config.aim_accel_zero,
        Some(SensorCalibration {
            x: -12,
            y: 34,
            z: 56,
            axes: SensorCalibration::ALL
        })
    );
    assert_eq!(
        config.aim_motion_plus_bias,
        Some(SensorCalibration {
            x: -3,
            y: 9,
            z: 7,
            axes: SensorCalibration::ALL
        })
    );
    config.validate().unwrap();
}

#[test]
fn grammar_ignores_comments_and_blank_lines_but_rejects_malformed_keys() {
    let mut config = Config::default();
    for (line, text) in [
        "",
        "   # comment",
        "\t# comment\r\n",
        " profile=gamepad # trailing",
    ]
    .into_iter()
    .enumerate()
    {
        config.apply_line("grammar", line + 1, text).unwrap();
    }
    err_line(
        config.apply_line("grammar", 10, "profile"),
        10,
        "expected key=value",
    );
    err_line(
        config.apply_line("grammar", 11, "unknown=value"),
        11,
        "unknown key",
    );
    err_line(
        config.apply_line("grammar", 12, "desktop.no-such=enter"),
        12,
        "invalid value",
    );
    err_line(
        config.apply_line("grammar", 13, "device..profile=desktop"),
        13,
        "invalid value",
    );
    assert!(config.apply_line("grammar", 14, "=gamepad").is_err());
}

#[test]
fn scalar_ranges_accept_boundaries_and_reject_adjacent_values() {
    let cases = [
        ("pointer-speed", 1, 127),
        ("ir-speed", 1, 127),
        ("ir-deadzone", 0, 127),
        ("ir-smoothing", 0, 95),
        ("aim-sensitivity", 1, 127),
        ("aim-deadzone", 0, 32_767),
        ("aim-smoothing", 0, 95),
        ("aim-calibration-duration", 1, 30),
    ];
    for (key, min, max) in cases {
        let mut config = Config::default();
        config
            .apply_line("range", 1, &format!("{key}={min}"))
            .unwrap();
        config
            .apply_line("range", 2, &format!("{key}={max}"))
            .unwrap();
        err_line(
            config.apply_line("range", 3, &format!("{key}={}", min - 1)),
            3,
            "invalid value",
        );
        err_line(
            config.apply_line("range", 4, &format!("{key}={}", max + 1)),
            4,
            "invalid value",
        );
        err_line(
            config.apply_line("range", 5, &format!("{key}=nope")),
            5,
            "invalid value",
        );
    }
    for key in [
        "ir-screen-left",
        "ir-screen-right",
        "ir-screen-top",
        "ir-screen-bottom",
    ] {
        let mut config = Config::default();
        config.apply_line("range", 1, &format!("{key}=0")).unwrap();
        config
            .apply_line("range", 2, &format!("{key}=32767"))
            .unwrap();
        err_line(
            config.apply_line("range", 3, &format!("{key}=-1")),
            3,
            "invalid value",
        );
        err_line(
            config.apply_line("range", 4, &format!("{key}=32768")),
            4,
            "invalid value",
        );
    }
    for key in [
        "aim-accel-zero-x",
        "aim-accel-zero-y",
        "aim-accel-zero-z",
        "aim-motion-plus-bias-x",
        "aim-motion-plus-bias-y",
        "aim-motion-plus-bias-z",
    ] {
        let mut config = Config::default();
        config
            .apply_line("range", 1, &format!("{key}=-32768"))
            .unwrap();
        config
            .apply_line("range", 2, &format!("{key}=32767"))
            .unwrap();
        err_line(
            config.apply_line("range", 3, &format!("{key}=-32769")),
            3,
            "invalid value",
        );
        err_line(
            config.apply_line("range", 4, &format!("{key}=32768")),
            4,
            "invalid value",
        );
    }
}

#[test]
fn choices_and_boolean_spellings_are_canonical() {
    for value in ["gamepad", "desktop", "both"] {
        let mut c = Config::default();
        c.apply_line("choice", 1, &format!("profile={value}"))
            .unwrap();
        assert_eq!(c.profile.as_str(), Some(value));
    }
    for (key, values) in [
        ("ir-tracking", ["first", "centroid", "dual"]),
        ("ir-aim-mapping", ["relative", "absolute", "relative"]),
        ("aim-mode", ["off", "mouse", "right-stick"]),
        ("aim-source", ["auto", "ir", "motion-plus"]),
        ("aim-activation", ["always", "b", "z"]),
    ] {
        for value in values {
            Config::default()
                .apply_line("choice", 1, &format!("{key}={value}"))
                .unwrap();
        }
    }
    for (key, values) in [
        ("aim-source", ["auto", "ir", "motion-plus", "accelerometer"]),
        ("aim-activation", ["always", "b", "z", "c"]),
    ] {
        for value in values {
            Config::default()
                .apply_line("choice", 1, &format!("{key}={value}"))
                .unwrap();
        }
    }
    for value in [
        "disabled",
        "left-click",
        "right-click",
        "enter",
        "escape",
        "overview",
        "page-up",
        "page-down",
    ] {
        assert!(DesktopAction::parse(value).is_some());
    }
    for value in ["yes", "true", "1", "no", "false", "0"] {
        let mut c = Config::default();
        c.apply_line("bool", 1, &format!("aim-invert-x={value}"))
            .unwrap();
    }
    let mut c = Config::default();
    err_line(
        c.apply_line("choice", 1, "backend=libei"),
        1,
        "invalid value",
    );
    for (key, value) in [
        ("profile", "bad"),
        ("ir-tracking", "bad"),
        ("ir-aim-mapping", "bad"),
        ("aim-mode", "bad"),
        ("aim-source", "bad"),
        ("aim-activation", "bad"),
        ("aim-invert-x", "maybe"),
    ] {
        err_line(
            c.apply_line("choice", 2, &format!("{key}={value}")),
            2,
            "invalid value",
        );
    }
}

#[test]
fn calibration_and_screen_validation_require_complete_non_inverted_values() {
    let mut c = Config::default();
    c.apply_line("validate", 1, "ir-screen-left=900").unwrap();
    c.apply_line("validate", 2, "ir-screen-right=100").unwrap();
    assert!(c.validate().unwrap_err().message.contains("right > left"));

    let mut c = Config::default();
    c.apply_line("validate", 1, "ir-screen-top=700").unwrap();
    c.apply_line("validate", 2, "ir-screen-bottom=100").unwrap();
    assert!(c.validate().is_err());

    for key in ["aim-accel-zero-x", "aim-motion-plus-bias-y"] {
        let mut c = Config::default();
        c.apply_line("validate", 1, &format!("{key}=0")).unwrap();
        assert!(c.validate().is_err());
    }
    let mut c = Config {
        aim_accel_zero: Some(SensorCalibration {
            x: 1,
            y: 2,
            z: 3,
            axes: 8,
        }),
        ..Config::default()
    };
    assert!(
        c.validate()
            .unwrap_err()
            .message
            .contains("invalid sensor calibration axes")
    );
    c.aim_accel_zero = Some(SensorCalibration {
        x: 1,
        y: 2,
        z: 3,
        axes: 0,
    });
    c.validate().unwrap();
}

#[test]
fn layers_have_system_then_user_order_and_explicit_file_wins_alone() {
    let system = write_temp("system", b"profile=desktop\npointer-speed=11\n");
    let user = write_temp("user", b"profile=both\npointer-speed=22\n");
    let explicit = write_temp("explicit", b"profile=gamepad\npointer-speed=33\n");
    let layered = Config::load_layers(Some(&system), Some(&user), Option::<&Path>::None).unwrap();
    assert_eq!(layered.profile, Profile::BOTH);
    assert_eq!(layered.pointer_speed, 22);
    let chosen = Config::load_layers(Some(&system), Some(&user), Some(&explicit)).unwrap();
    assert_eq!(chosen.profile, Profile::GAMEPAD);
    assert_eq!(chosen.pointer_speed, 33);
    remove(&system);
    remove(&user);
    remove(&explicit);

    let missing_optional = Config::load_layers(
        Some(temp_path("not-there-system")),
        None::<&Path>,
        None::<&Path>,
    )
    .unwrap();
    assert_eq!(missing_optional, Config::default());
    let missing = temp_path("not-there-explicit");
    let error = Config::load_layers(None::<&Path>, None::<&Path>, Some(&missing)).unwrap_err();
    assert_eq!(error.path, missing);
}

#[test]
fn rules_match_substrings_and_duplicate_updates_move_to_end() {
    let mut c = Config::default();
    c.apply_line("rules", 1, "device.blue.profile=desktop")
        .unwrap();
    c.apply_line("rules", 2, "device.wiimote.profile=both")
        .unwrap();
    c.apply_line("rules", 3, "device.blue.profile=gamepad")
        .unwrap();
    c.apply_line("rules", 4, "device-type.balanceboard.profile=desktop")
        .unwrap();
    assert_eq!(c.device_rules.len(), 3);
    assert_eq!(c.device_rules[0].match_text, "wiimote");
    assert_eq!(c.device_rules[1].match_text, "blue");
    assert_eq!(c.device_rules[2].kind, DeviceRuleKind::Devtype);
    assert_eq!(
        c.profile_for_syspath("/sys/devices/blue/wiimote"),
        Profile::GAMEPAD
    );
    assert_eq!(
        c.profile_for_syspath("/sys/devices/red/wiimote"),
        Profile::BOTH
    );
    assert_eq!(
        c.profile_for_device(Some("/sys/devices/red/wiimote"), Some("balanceboard")),
        Profile::DESKTOP
    );
    assert_eq!(
        c.profile_for_device(Some("/sys/devices/red/wiimote"), Some("procontroller")),
        Profile::BOTH
    );
    assert_eq!(
        c.profile_for_device(None, Some("balanceboard")),
        Profile::DESKTOP
    );
    assert_eq!(c.profile_for_device(None, None), Profile::GAMEPAD);

    let mut full = Config::default();
    for i in 0..MAX_DEVICE_RULES {
        full.apply_line("rules", i + 1, &format!("device.rule-{i}.profile=gamepad"))
            .unwrap();
    }
    err_line(
        full.apply_line("rules", 100, "device.one-more.profile=desktop"),
        100,
        "invalid value",
    );
}

#[test]
fn dump_is_canonical_order_and_round_trips_complete_values() {
    let mut c = Config::default();
    for line in [
        "profile=both",
        "pointer-speed=31",
        "ir-speed=16",
        "ir-deadzone=4",
        "ir-smoothing=25",
        "ir-tracking=centroid",
        "ir-aim-mapping=absolute",
        "ir-screen-left=12",
        "ir-screen-right=900",
        "ir-screen-top=34",
        "ir-screen-bottom=700",
        "aim-mode=right-stick",
        "aim-source=motion-plus",
        "aim-activation=z",
        "aim-sensitivity=24",
        "aim-deadzone=12",
        "aim-smoothing=40",
        "aim-invert-x=yes",
        "aim-invert-y=yes",
        "aim-accel-zero-x=-12",
        "aim-accel-zero-y=34",
        "aim-accel-zero-z=56",
        "aim-motion-plus-bias-x=-3",
        "aim-motion-plus-bias-y=9",
        "aim-motion-plus-bias-z=7",
        "aim-calibration-duration=12",
        "desktop.a=enter",
        "device.blue.profile=desktop",
    ] {
        c.apply_line("dump", 1, line).unwrap();
    }
    let dump = c.dump();
    let keys: Vec<_> = dump
        .lines()
        .map(|line| line.split('=').next().unwrap())
        .collect();
    assert_eq!(
        keys,
        vec![
            "backend",
            "profile",
            "pointer-speed",
            "ir-speed",
            "ir-deadzone",
            "ir-smoothing",
            "ir-tracking",
            "ir-aim-mapping",
            "ir-screen-left",
            "ir-screen-right",
            "ir-screen-top",
            "ir-screen-bottom",
            "aim-mode",
            "aim-source",
            "aim-activation",
            "aim-sensitivity",
            "aim-deadzone",
            "aim-smoothing",
            "aim-invert-x",
            "aim-invert-y",
            "aim-accel-zero-x",
            "aim-accel-zero-y",
            "aim-accel-zero-z",
            "aim-motion-plus-bias-x",
            "aim-motion-plus-bias-y",
            "aim-motion-plus-bias-z",
            "aim-calibration-duration",
            "desktop.a",
            "desktop.b",
            "desktop.plus",
            "desktop.minus",
            "desktop.home",
            "desktop.one",
            "desktop.two",
            "device.blue.profile",
        ]
    );
    assert!(dump.ends_with('\n'));
    let path = write_temp("dump-round-trip", dump.as_bytes());
    let round_trip = Config::load_file(&path).unwrap();
    remove(&path);
    assert_eq!(round_trip, c);
}

#[test]
fn file_loader_enforces_line_size_and_utf8() {
    let long = write_temp("long-line", &vec![b'x'; MAX_LINE_BYTES]);
    let error = Config::load_file(&long).unwrap_err();
    assert_eq!(error.line, Some(1));
    assert!(error.message.contains("line too long"));
    remove(&long);

    let invalid = write_temp("invalid-utf8", b"profile=gamepad\n\xff\n");
    let error = Config::load_file(&invalid).unwrap_err();
    assert_eq!(error.line, Some(2));
    assert!(error.message.contains("invalid UTF-8"));
    remove(&invalid);

    let accepted = write_temp(
        "max-line",
        format!("#{}", "x".repeat(MAX_LINE_BYTES - 2)).as_bytes(),
    );
    Config::load_file(&accepted).unwrap();
    remove(&accepted);
}

#[test]
fn byte_parser_and_file_loader_share_line_boundaries_and_errors() {
    for bytes in [
        b"profile=desktop\r\n".to_vec(),
        b"profile=desktop".to_vec(),
        vec![b'#'; 512],
        vec![0xff],
        b"pointer-speed=0\n".to_vec(),
    ] {
        let path =
            std::env::temp_dir().join(format!("wiiland-parser-parity-{}", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let parsed = wiiland_core::Config::parse_bytes(&path, &bytes);
        let loaded = wiiland_core::Config::load_file(&path);
        match (parsed, loaded) {
            (Ok(a), Ok(b)) => assert_eq!(a, b),
            (Err(a), Err(b)) => {
                assert_eq!(a.line, b.line);
                assert_eq!(a.message, b.message);
            }
            pair => panic!("parser disagreement: {pair:?}"),
        }
        std::fs::remove_file(path).unwrap();
    }
}
