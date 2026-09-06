use std::str::FromStr;

use wiiland_core::SensorCalibration;
use wiiland_core::calibration::{
    AIM_CALIBRATION_MAX_JITTER, AIM_CALIBRATION_MIN_SAMPLES, CalibrationStats,
    SENSOR_CALIBRATION_ALL, SENSOR_CALIBRATION_X, SENSOR_CALIBRATION_Y, SENSOR_CALIBRATION_Z,
    calibration_jitter, calibration_stats_finish, sensor_calibration_ready,
};
use wiiland_core::trace::{
    AbsPayload, EventType, KeyPayload, TraceConfig, TraceEvent, TraceFilter, TracePayload,
    event_type_name, is_abs_event, is_key_event,
};

#[test]
fn extreme_sensor_values_reject_an_unstable_window_without_overflow() {
    let mut stats = CalibrationStats::new();
    for _ in 0..AIM_CALIBRATION_MIN_SAMPLES {
        stats.add([i32::MIN, 0, i32::MAX]);
        stats.add([i32::MAX, 0, i32::MIN]);
    }
    assert_eq!(stats.jitter(), i32::MAX);
    assert!(stats.finish().is_none());
}

#[test]
fn calibration_accumulates_extrema_and_finishes_at_stability_boundaries() {
    let mut stats = CalibrationStats::new();
    assert_eq!(stats.samples, 0);
    assert_eq!(stats.jitter(), 0);
    assert_eq!(calibration_jitter(&stats), 0);
    assert_eq!(stats.finish(), None);
    stats.add([10, -20, 3]);
    stats.add([-2, -18, 8]);
    assert_eq!(stats.samples, 2);
    assert_eq!(stats.sum_x, 8);
    assert_eq!(stats.sum_y, -38);
    assert_eq!(stats.sum_z, 11);
    assert_eq!((stats.min_x, stats.max_x), (-2, 10));
    assert_eq!((stats.min_y, stats.max_y), (-20, -18));
    assert_eq!((stats.min_z, stats.max_z), (3, 8));
    assert_eq!(stats.jitter(), 12);

    let mut enough = CalibrationStats::new();
    for _ in 0..(AIM_CALIBRATION_MIN_SAMPLES - 1) {
        enough.add([100, -200, 300]);
    }
    assert_eq!(enough.finish(), None);
    enough.add([100, -200, 300]);
    assert_eq!(
        enough.finish(),
        Some(SensorCalibration {
            x: 100,
            y: -200,
            z: 300,
            axes: SENSOR_CALIBRATION_ALL
        })
    );

    let mut edge = CalibrationStats::new();
    for i in 0..AIM_CALIBRATION_MIN_SAMPLES {
        edge.add([
            if i == 0 {
                0
            } else {
                AIM_CALIBRATION_MAX_JITTER
            },
            10,
            20,
        ]);
    }
    assert_eq!(edge.jitter(), AIM_CALIBRATION_MAX_JITTER);
    assert!(calibration_stats_finish(&edge).is_some());
    edge.add([AIM_CALIBRATION_MAX_JITTER + 1, 10, 20]);
    assert_eq!(edge.jitter(), AIM_CALIBRATION_MAX_JITTER + 1);
    assert_eq!(edge.finish(), None);
}

#[test]
fn calibration_means_use_signed_integer_truncation_and_clear_resets_state() {
    let mut stats = CalibrationStats::new();
    for i in 0..16 {
        stats.add([if i == 0 { -17 } else { 0 }, if i == 0 { 17 } else { 0 }, 1]);
    }
    assert_eq!(
        stats.finish(),
        Some(SensorCalibration {
            x: -1,
            y: 1,
            z: 1,
            axes: SENSOR_CALIBRATION_ALL
        })
    );
    stats.clear();
    assert_eq!(stats, CalibrationStats::new());
    stats.add([1, 2, 3]);
    assert_eq!(stats.jitter(), 0);
}

#[test]
fn sensor_calibration_requires_exactly_all_three_axes() {
    assert!(!sensor_calibration_ready(&SensorCalibration {
        x: 0,
        y: 0,
        z: 0,
        axes: 0
    }));
    assert!(!sensor_calibration_ready(&SensorCalibration {
        x: 1,
        y: 2,
        z: 3,
        axes: SENSOR_CALIBRATION_X | SENSOR_CALIBRATION_Y
    }));
    assert!(sensor_calibration_ready(&SensorCalibration {
        x: 1,
        y: 2,
        z: 3,
        axes: SENSOR_CALIBRATION_ALL
    }));
    assert!(!sensor_calibration_ready(&SensorCalibration {
        x: 1,
        y: 2,
        z: 3,
        axes: SENSOR_CALIBRATION_ALL | 8
    }));
    assert_eq!(
        SENSOR_CALIBRATION_ALL,
        SENSOR_CALIBRATION_X | SENSOR_CALIBRATION_Y | SENSOR_CALIBRATION_Z
    );
}

#[test]
fn event_names_and_classification_cover_all_trace_event_types() {
    let expected = [
        (EventType::Key, "key", true, false),
        (EventType::Accelerometer, "accelerometer", false, true),
        (EventType::Ir, "ir", false, true),
        (EventType::BalanceBoard, "balance-board", false, true),
        (EventType::MotionPlus, "motion-plus", false, true),
        (EventType::ProKey, "pro-key", true, false),
        (EventType::ProMove, "pro-move", false, true),
        (EventType::Watch, "watch", false, false),
        (EventType::ClassicKey, "classic-key", true, false),
        (EventType::ClassicMove, "classic-move", false, true),
        (EventType::NunchukKey, "nunchuk-key", true, false),
        (EventType::NunchukMove, "nunchuk-move", false, true),
        (EventType::DrumsKey, "drums-key", true, false),
        (EventType::DrumsMove, "drums-move", false, true),
        (EventType::GuitarKey, "guitar-key", true, false),
        (EventType::GuitarMove, "guitar-move", false, true),
        (EventType::Gone, "gone", false, false),
    ];
    for (raw, (event, name, key, axes)) in expected.into_iter().enumerate() {
        assert_eq!(event.raw(), raw as u32);
        assert_eq!(EventType::from_raw(raw as u32), event);
        assert_eq!(event_type_name(raw as u32), name);
        assert_eq!(is_key_event(raw as u32), key);
        assert_eq!(is_abs_event(raw as u32), axes);
    }
    assert_eq!(EventType::from_raw(999), EventType::Unknown(999));
    assert_eq!(event_type_name(999), "unknown");
    assert!(!is_key_event(999) && !is_abs_event(999));
}

#[test]
fn trace_filters_parse_and_match_categories() {
    for (name, filter) in [
        ("all", TraceFilter::All),
        ("keys", TraceFilter::Keys),
        ("axes", TraceFilter::Axes),
        ("ir", TraceFilter::Ir),
        ("motion-plus", TraceFilter::MotionPlus),
    ] {
        assert_eq!(TraceFilter::from_str(name), Ok(filter));
        assert_eq!(filter.name(), name);
    }
    assert_eq!(TraceFilter::from_str("bad"), Err(()));
    for event in [EventType::Key, EventType::ClassicKey, EventType::ProKey] {
        assert!(TraceFilter::Keys.matches(event.raw()));
    }
    for event in [EventType::Ir, EventType::MotionPlus, EventType::ProMove] {
        assert!(TraceFilter::Axes.matches(event.raw()));
    }
    assert!(TraceFilter::Ir.matches(EventType::Ir.raw()));
    assert!(!TraceFilter::Ir.matches(EventType::Accelerometer.raw()));
    assert!(TraceFilter::MotionPlus.matches(EventType::MotionPlus.raw()));
    assert!(!TraceFilter::MotionPlus.matches(EventType::ProMove.raw()));
    assert!(TraceFilter::All.matches(999));
}

#[test]
fn trace_event_format_is_deterministic_for_time_and_payload_variants() {
    let key = TraceEvent::new(
        7,
        Some(1_234_567),
        "/sys/wii0",
        EventType::Key.raw(),
        TracePayload::Key(KeyPayload { code: 5, state: 1 }),
    );
    assert_eq!(
        key.format_line(),
        "time=1.234567 seq=7 /sys/wii0 key type=0 key=5 state=1\n"
    );
    assert_eq!(key.to_string(), key.format_line());

    let axes = TraceEvent::new(
        8,
        None,
        "wiimote",
        EventType::MotionPlus.raw(),
        TracePayload::Axes(vec![
            AbsPayload { x: -1, y: 2, z: 3 },
            AbsPayload { x: 4, y: 5, z: 6 },
        ]),
    );
    assert_eq!(
        axes.format_line(),
        "time=unknown seq=8 wiimote motion-plus type=4 abs0=-1,2,3 abs1=4,5,6\n"
    );

    let none = TraceEvent::new(
        9,
        Some(-1),
        "gone",
        EventType::Gone.raw(),
        TracePayload::None,
    );
    assert_eq!(
        none.format_line(),
        "time=-1.999999 seq=9 gone gone type=16\n"
    );
}

#[test]
fn trace_config_requires_enablement_and_honors_optional_selector() {
    let mut config = TraceConfig::default();
    assert!(!config.enabled);
    assert!(!config.matches(EventType::Key.raw()));
    config.enable(None);
    assert!(config.enabled && config.filter == TraceFilter::All);
    assert!(config.matches(EventType::Key.raw()));
    config.enable(Some(TraceFilter::MotionPlus));
    assert!(config.matches(EventType::MotionPlus.raw()));
    assert!(!config.matches(EventType::Ir.raw()));
    config.enable(None);
    assert_eq!(config.filter, TraceFilter::MotionPlus);
}
