use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use wiiland_ipc::{
    Axis3, ButtonEvent, Command, DeviceInfo, FrameBuffer, InputPayload, Notification, Profile,
    ProtocolError, ProtocolErrorCode, RemovalReason, Request, ResponseResult, ServerMessage,
    Status, Subscription, Timestamp, decode_frame, encode_frame,
};

fn assert_wire<T>(value: &T, expected: Value)
where
    T: Serialize + DeserializeOwned,
{
    assert_eq!(serde_json::to_value(value).unwrap(), expected);
    let decoded: T = serde_json::from_value(expected.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
}

fn device() -> DeviceInfo {
    DeviceInfo {
        syspath: "/sys/devices/mock-wiimote".to_owned(),
        profile: Profile::Both,
        opened_interfaces: 2,
        pending_interfaces: 1,
        gamepad_output: true,
        desktop_output: false,
    }
}

fn axes<const N: usize>() -> [Axis3; N] {
    std::array::from_fn(|index| Axis3 {
        x: index as i32,
        y: index as i32 + 1,
        z: index as i32 + 2,
    })
}

#[test]
fn protocol_one_hello_request_and_correlated_response_error_have_exact_shapes() {
    assert_eq!(wiiland_ipc::PROTOCOL_MAJOR, 1);
    assert_eq!(wiiland_ipc::PROTOCOL_MINOR, 1);

    let request = Request {
        id: 41,
        command: Command::Hello {
            min_major: 1,
            max_major: 1,
        },
    };
    assert_wire(
        &request,
        json!({
            "id": 41,
            "command": {"type": "hello", "min_major": 1, "max_major": 1}
        }),
    );

    let response = ServerMessage::Response {
        id: 41,
        result: ResponseResult::Hello {
            major: 1,
            minor: 0,
            daemon_version: "0.4.2".to_owned(),
        },
    };
    assert_wire(
        &response,
        json!({
            "type": "response",
            "value": {
                "id": 41,
                "result": {
                    "type": "hello",
                    "value": {"major": 1, "minor": 0, "daemon_version": "0.4.2"}
                }
            }
        }),
    );

    let error = ServerMessage::Error {
        id: Some(41),
        error: ProtocolError {
            code: ProtocolErrorCode::UnsupportedVersion,
            message: "no supported protocol major".to_owned(),
        },
    };
    assert_wire(
        &error,
        json!({
            "type": "error",
            "value": {
                "id": 41,
                "error": {"code": "unsupported_version", "message": "no supported protocol major"}
            }
        }),
    );
}

#[test]
fn scalar_enums_use_snake_case_wire_vocabulary() {
    for (value, wire) in [
        (Profile::None, "none"),
        (Profile::Gamepad, "gamepad"),
        (Profile::Desktop, "desktop"),
        (Profile::Both, "both"),
        (Profile::Unknown, "unknown"),
    ] {
        assert_wire(&value, json!(wire));
    }
    for (value, wire) in [
        (Subscription::All, "all"),
        (Subscription::Input, "input"),
        (Subscription::Devices, "devices"),
    ] {
        assert_wire(&value, json!(wire));
    }
    for (value, wire) in [
        (RemovalReason::Removed, "removed"),
        (RemovalReason::Gone, "gone"),
        (RemovalReason::DrainError, "drain_error"),
        (RemovalReason::PointerError, "pointer_error"),
        (RemovalReason::Unknown, "unknown"),
    ] {
        assert_wire(&value, json!(wire));
    }
    for (value, wire) in [
        (ProtocolErrorCode::UnsupportedVersion, "unsupported_version"),
        (ProtocolErrorCode::InvalidRequest, "invalid_request"),
        (ProtocolErrorCode::UnknownCommand, "unknown_command"),
        (ProtocolErrorCode::NotSubscribed, "not_subscribed"),
        (ProtocolErrorCode::FrameTooLarge, "frame_too_large"),
        (ProtocolErrorCode::InvalidJson, "invalid_json"),
        (ProtocolErrorCode::Internal, "internal"),
        (ProtocolErrorCode::Unknown, "unknown"),
    ] {
        assert_wire(&value, json!(wire));
    }
}

#[test]
fn status_and_device_info_are_stable_objects() {
    let status = Status {
        daemon_version: "0.4.2".to_owned(),
        pid: 1234,
        device_count: 1,
        dry_run: true,
        socket_path: "/run/user/1000/wiiland.sock".to_owned(),
    };
    assert_wire(
        &status,
        json!({
            "daemon_version": "0.4.2",
            "pid": 1234,
            "device_count": 1,
            "dry_run": true,
            "socket_path": "/run/user/1000/wiiland.sock"
        }),
    );

    assert_wire(
        &device(),
        json!({
            "syspath": "/sys/devices/mock-wiimote",
            "profile": "both",
            "opened_interfaces": 2,
            "pending_interfaces": 1,
            "gamepad_output": true,
            "desktop_output": false
        }),
    );
}

#[test]
fn subscription_commands_and_results_have_exact_shapes() {
    let subscribe = Request {
        id: 7,
        command: Command::Subscribe {
            subscriptions: vec![
                Subscription::All,
                Subscription::Input,
                Subscription::Devices,
            ],
        },
    };
    assert_wire(
        &subscribe,
        json!({
            "id": 7,
            "command": {
                "type": "subscribe",
                "subscriptions": ["all", "input", "devices"]
            }
        }),
    );

    let unsubscribe = Request {
        id: 8,
        command: Command::Unsubscribe {
            subscriptions: vec![Subscription::Input, Subscription::Devices],
        },
    };
    assert_wire(
        &unsubscribe,
        json!({
            "id": 8,
            "command": {
                "type": "unsubscribe",
                "subscriptions": ["input", "devices"]
            }
        }),
    );

    assert_wire(
        &ServerMessage::Response {
            id: 7,
            result: ResponseResult::Subscribed,
        },
        json!({
            "type": "response",
            "value": {"id": 7, "result": {"type": "subscribed"}}
        }),
    );
    assert_wire(
        &ServerMessage::Response {
            id: 8,
            result: ResponseResult::Unsubscribed,
        },
        json!({
            "type": "response",
            "value": {"id": 8, "result": {"type": "unsubscribed"}}
        }),
    );
}

#[test]
fn all_notification_families_preserve_sequence_and_removal_reason() {
    let added = ServerMessage::Notification(Notification::DeviceAdded {
        sequence: 10,
        device: device(),
    });
    assert_wire(
        &added,
        json!({
            "type": "notification",
            "value": {
                "type": "device_added",
                "sequence": 10,
                "device": {
                    "syspath": "/sys/devices/mock-wiimote",
                    "profile": "both",
                    "opened_interfaces": 2,
                    "pending_interfaces": 1,
                    "gamepad_output": true,
                    "desktop_output": false
                }
            }
        }),
    );

    let removed = ServerMessage::Notification(Notification::DeviceRemoved {
        sequence: 11,
        syspath: "/sys/devices/mock-wiimote".to_owned(),
        reason: RemovalReason::DrainError,
    });
    assert_wire(
        &removed,
        json!({
            "type": "notification",
            "value": {
                "type": "device_removed",
                "sequence": 11,
                "syspath": "/sys/devices/mock-wiimote",
                "reason": "drain_error"
            }
        }),
    );

    let input = ServerMessage::Notification(Notification::Input {
        sequence: 12,
        syspath: "/sys/devices/mock-wiimote".to_owned(),
        timestamp: Timestamp {
            seconds: 123,
            micros: 456,
        },
        payload: InputPayload::Key(ButtonEvent { code: 28, state: 1 }),
    });
    assert_wire(
        &input,
        json!({
            "type": "notification",
            "value": {
                "type": "input",
                "sequence": 12,
                "syspath": "/sys/devices/mock-wiimote",
                "timestamp": {"seconds": 123, "micros": 456},
                "payload": {"type": "key", "value": {"code": 28, "state": 1}}
            }
        }),
    );
}

#[test]
fn representative_input_payloads_have_tagged_shapes() {
    assert_wire(
        &InputPayload::Key(ButtonEvent {
            code: BTN_A,
            state: 1,
        }),
        json!({"type": "key", "value": {"code": BTN_A, "state": 1}}),
    );
    assert_wire(
        &InputPayload::Accel(Axis3 {
            x: -1,
            y: 2,
            z: 300,
        }),
        json!({"type": "accel", "value": {"x": -1, "y": 2, "z": 300}}),
    );
    assert_wire(
        &InputPayload::Unknown(0xfeed),
        json!({"type": "unknown", "value": 65261}),
    );
}

#[test]
fn all_supported_input_payloads_roundtrip() {
    let payloads = [
        InputPayload::Key(ButtonEvent {
            code: BTN_A,
            state: 1,
        }),
        InputPayload::Accel(Axis3 { x: 1, y: 2, z: 3 }),
        InputPayload::Ir(axes()),
        InputPayload::BalanceBoard(axes()),
        InputPayload::MotionPlus(Axis3 { x: 1, y: 2, z: 3 }),
        InputPayload::ProControllerKey(ButtonEvent {
            code: BTN_A,
            state: 0,
        }),
        InputPayload::ProControllerMove(axes()),
        InputPayload::Watch,
        InputPayload::ClassicControllerKey(ButtonEvent {
            code: BTN_A,
            state: 1,
        }),
        InputPayload::ClassicControllerMove(axes()),
        InputPayload::NunchukKey(ButtonEvent {
            code: BTN_A,
            state: 0,
        }),
        InputPayload::NunchukMove(axes()),
        InputPayload::DrumsKey(ButtonEvent {
            code: BTN_A,
            state: 1,
        }),
        InputPayload::DrumsMove(axes()),
        InputPayload::GuitarKey(ButtonEvent {
            code: BTN_A,
            state: 0,
        }),
        InputPayload::GuitarMove(axes()),
        InputPayload::Gone,
        InputPayload::Unknown(0xfeed),
    ];

    for payload in payloads {
        let encoded = serde_json::to_value(&payload).unwrap();
        let decoded = serde_json::from_value(encoded).unwrap();
        assert_eq!(payload, decoded);
    }
}

#[test]
fn unknown_notification_and_input_tags_deserialize_as_unsupported() {
    let notification: Notification = serde_json::from_value(json!({
        "type": "battery_changed",
        "sequence": 13,
        "percentage": 82
    }))
    .unwrap();
    assert_eq!(notification, Notification::Unsupported);
    assert_wire(
        &notification,
        json!({
            "type": "unsupported"
        }),
    );

    let payload: InputPayload = serde_json::from_value(json!({
        "type": "pressure",
        "value": {
            "amount": 7
        }
    }))
    .unwrap();
    assert_eq!(payload, InputPayload::Unsupported);

    let unit_payload: InputPayload =
        serde_json::from_value(json!({"type": "future_unit"})).unwrap();
    assert_eq!(unit_payload, InputPayload::Unsupported);
    assert_wire(
        &payload,
        json!({
            "type": "unsupported"
        }),
    );
}

#[test]
fn known_input_tags_reject_missing_or_wrong_values() {
    assert!(serde_json::from_value::<InputPayload>(json!({"type": "key"})).is_err());
    assert!(
        serde_json::from_value::<InputPayload>(json!({
            "type": "key",
            "value": "not-a-button"
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<InputPayload>(json!({
            "type": "watch",
            "value": {"unexpected": true}
        }))
        .is_err()
    );
}

#[test]
fn unknown_object_fields_are_accepted_without_changing_known_wire_shape() {
    let request: Request = serde_json::from_value(json!({
        "id": 9,
        "command": {
            "type": "hello",
            "min_major": 1,
            "max_major": 1,
            "future_command_field": {"enabled": true}
        },
        "future_request_field": "ignored"
    }))
    .unwrap();
    assert_wire(
        &request,
        json!({
            "id": 9,
            "command": {"type": "hello", "min_major": 1, "max_major": 1}
        }),
    );

    let status: Status = serde_json::from_value(json!({
        "daemon_version": "0.4.2",
        "pid": 5,
        "device_count": 0,
        "dry_run": false,
        "socket_path": "/tmp/wiiland.sock",
        "future_status": [1, 2, 3]
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_value(status).unwrap(),
        json!({
            "daemon_version": "0.4.2",
            "pid": 5,
            "device_count": 0,
            "dry_run": false,
            "socket_path": "/tmp/wiiland.sock"
        })
    );

    let info: DeviceInfo = serde_json::from_value(json!({
        "syspath": "/sys/devices/new",
        "profile": "gamepad",
        "opened_interfaces": 1,
        "pending_interfaces": 0,
        "gamepad_output": true,
        "desktop_output": true,
        "future_device_field": null
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_value(info).unwrap(),
        json!({
            "syspath": "/sys/devices/new",
            "profile": "gamepad",
            "opened_interfaces": 1,
            "pending_interfaces": 0,
            "gamepad_output": true,
            "desktop_output": true
        })
    );
}

#[test]
fn unknown_command_deserializes_to_unknown_variant() {
    let request: Request = serde_json::from_value(json!({
        "id": 99,
        "command": {"type": "future_command", "argument": 7}
    }))
    .unwrap();
    assert!(matches!(request.command, Command::Unknown));
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        json!({"id": 99, "command": {"type": "unknown"}})
    );
}

#[test]
fn frame_buffer_handles_lf_and_crlf_using_public_api() {
    let request = Request {
        id: 3,
        command: Command::Ping,
    };
    let lf = encode_frame(&request).unwrap();
    assert_eq!(lf.last(), Some(&b'\n'));

    let mut crlf = serde_json::to_vec(&request).unwrap();
    crlf.extend_from_slice(b"\r\n");
    let mut stream = lf.clone();
    stream.extend_from_slice(&crlf);

    let mut buffer = FrameBuffer::new();
    let split = 2;
    assert!(buffer.push(&stream[..split]).unwrap().is_empty());
    let frames = buffer.feed(&stream[split..]).unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0], serde_json::to_vec(&request).unwrap());
    assert_eq!(frames[1], serde_json::to_vec(&request).unwrap());
    assert!(buffer.is_empty());
    assert_eq!(buffer.len(), 0);
    assert_eq!(decode_frame::<Request>(&lf).unwrap().id, 3);
    assert_eq!(decode_frame::<Request>(&crlf).unwrap().id, 3);
}

const BTN_A: u32 = 304;

#[test]
fn minor_one_controls_are_additive_and_have_exact_wire_shapes() {
    assert_wire(&Command::Diagnostics, json!({"type":"diagnostics"}));
    assert_wire(&Command::Config, json!({"type":"config"}));
    assert_wire(
        &Command::StartCapture {
            syspath: "/sys/test".into(),
        },
        json!({"type":"start_capture","syspath":"/sys/test"}),
    );
    assert_wire(&Command::StopCapture, json!({"type":"stop_capture"}));
    assert_wire(
        &ResponseResult::Config("profile=gamepad\n".into()),
        json!({"type":"config","value":"profile=gamepad\n"}),
    );
    assert_wire(
        &ResponseResult::CaptureStopped,
        json!({"type":"capture_stopped"}),
    );
    assert_wire(
        &ResponseResult::Diagnostics(wiiland_ipc::Diagnostics::default()),
        json!({"type":"diagnostics","value":{"trace_records_dropped":0,"lifecycle_records_dropped":0,"max_pointer_lateness_us":0,"max_dispatch_duration_us":0}}),
    );
}
