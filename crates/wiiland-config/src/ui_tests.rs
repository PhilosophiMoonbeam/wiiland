//! Exercise the real egui layout and input handling without a display server or hardware.
use super::*;

struct Harness {
    ctx: egui::Context,
    app: ControlCenter,
    size: egui::Vec2,
    output: egui::FullOutput,
}

impl Harness {
    fn new(size: [f32; 2]) -> Self {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut harness = Self {
            ctx,
            app: ControlCenter::new(ConfigModel::new(PathBuf::from("/tmp/wiiland-ui-test.conf"))),
            size: size.into(),
            output: egui::FullOutput::default(),
        };
        harness.frame(Vec::new());
        harness.frame(Vec::new());
        harness
    }

    fn frame(&mut self, events: Vec<egui::Event>) {
        self.output = self.ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, self.size)),
                events,
                ..Default::default()
            },
            |ctx| self.app.draw(ctx),
        );
    }

    fn settle(&mut self) {
        for _ in 0..3 {
            self.frame(Vec::new());
        }
    }

    fn texts(&self, text: &str) -> Vec<egui::Rect> {
        self.output
            .shapes
            .iter()
            .filter_map(|shape| {
                if let egui::Shape::Text(t) = &shape.shape {
                    let rect = egui::Rect::from_min_size(t.pos, t.galley.size());
                    if t.galley.text() == text && shape.clip_rect.contains_rect(rect) {
                        return Some(rect);
                    }
                }
                None
            })
            .collect()
    }

    fn click(&mut self, text: &str, index: usize) {
        let rects = self.texts(text);
        let pos = rects
            .get(index)
            .unwrap_or_else(|| panic!("Missing visible text {text:?} at index {index}"))
            .center();
        self.frame(vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        self.frame(vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        self.settle();
    }
}

#[test]
fn save_actions_remain_visible_with_long_forms_and_open_log() {
    for size in [[760.0, 600.0], [1180.0, 780.0]] {
        for dark in [false, true] {
            let mut h = Harness::new(size);
            h.ctx.set_theme(if dark {
                egui::Theme::Dark
            } else {
                egui::Theme::Light
            });
            h.app.tab = Tab::Configuration;
            h.app.model.dirty = true;
            h.app.output_open = true;
            h.app.model.config.device_rules = (0..model::MAX_DEVICE_RULES)
                .map(|i| {
                    model::rule(
                        DeviceRuleKind::Devtype,
                        format!("remote-{i}"),
                        Profile::GAMEPAD,
                    )
                })
                .collect();
            for section in ConfigSection::ALL {
                h.app.config_section = section;
                h.settle();
                for text in [
                    "Validate and save",
                    "Save and restart",
                    "Unsaved changes",
                    "Reload",
                ] {
                    assert_eq!(
                        h.texts(text).len(),
                        1,
                        "{text} clipped at {size:?}, {section:?}, dark={dark}"
                    );
                }
            }
        }
    }
}

#[test]
fn each_button_binding_has_a_visible_label() {
    let mut h = Harness::new([1180.0, 900.0]);
    h.app.tab = Tab::Configuration;
    h.app.config_section = ConfigSection::Bindings;
    h.settle();
    let first_label_x = h.texts("A button")[0].left();
    for (_, label) in model::binding_names() {
        assert!(
            (h.texts(label)[0].left() - first_label_x).abs() < 1.0,
            "Binding labels must align on the left"
        );
        assert_eq!(h.texts(label).len(), 1, "Missing button label: {label}");
    }
    let first_control_x = h.texts("Left click")[0].left();
    for text in ["Right click", "Enter", "Escape", "Page up", "Page down"] {
        assert!(
            (h.texts(text)[0].left() - first_control_x).abs() < 1.0,
            "Binding controls must share one column"
        );
    }
}

#[test]
fn second_rule_dropdown_edits_only_the_second_rule() {
    let mut h = Harness::new([1180.0, 1000.0]);
    h.app.tab = Tab::Configuration;
    h.app.config_section = ConfigSection::Rules;
    h.app.model.config.device_rules = vec![
        model::rule(
            DeviceRuleKind::Devtype,
            "first".to_owned(),
            Profile::GAMEPAD,
        ),
        model::rule(
            DeviceRuleKind::Devtype,
            "second".to_owned(),
            Profile::GAMEPAD,
        ),
    ];
    h.settle();
    h.click("Gamepad", 1);
    h.click("Desktop pointer", 0);
    assert_eq!(h.app.model.config.device_rules[0].profile, Profile::GAMEPAD);
    assert_eq!(h.app.model.config.device_rules[1].profile, Profile::DESKTOP);
    assert!(h.app.model.dirty);
}

#[test]
fn reload_confirmation_preserves_edits_when_cancelled() {
    let mut h = Harness::new([760.0, 600.0]);
    h.app.tab = Tab::Configuration;
    h.app.model.config.pointer_speed = 57;
    h.app.model.mark_dirty();
    h.settle();
    h.click("Reload", 0);
    assert!(h.app.reload_confirmation);
    assert!(h.app.config_task.is_none());
    h.click("Keep editing", 0);
    assert!(!h.app.reload_confirmation);
    assert!(h.app.model.dirty);
    assert_eq!(h.app.model.config.pointer_speed, 57);
    assert!(h.app.config_task.is_none());
}

#[test]
fn invalid_rule_is_explained_and_cannot_be_saved() {
    let mut h = Harness::new([760.0, 600.0]);
    h.app.tab = Tab::Configuration;
    h.app.config_section = ConfigSection::Rules;
    h.settle();
    h.click("Add rule", 0);
    assert!(h.app.model.dirty);
    assert!(h.app.model.validate_form().is_err());
    h.click("Validate and save", 0);
    assert!(h.app.config_task.is_none());
    assert!(h.output.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(t) if t.galley.text().starts_with("Before saving:"))));
}

#[test]
fn compact_navigation_reaches_validation_and_opens_activity_log() {
    let mut h = Harness::new([760.0, 600.0]);
    h.click("Test & calibrate", 0);
    assert_eq!(h.app.tab, Tab::Validation);
    assert_eq!(h.texts("Live input trace").len(), 1);
    assert_eq!(
        h.texts("Start trace").len(),
        1,
        "Start must be visible in the compact layout"
    );
    h.click("Activity log · 0", 0);
    assert!(h.app.output_open);
    assert_eq!(h.texts("Copy all").len(), 1);
    h.click("Hide log", 0);
    assert!(!h.app.output_open);
}

#[test]
fn service_status_distinguishes_stopped_from_unavailable() {
    for (success, code, stdout, expected) in [
        (true, Some(0), "active\n", "Running"),
        (false, Some(3), "inactive\n", "Stopped"),
        (false, Some(3), "failed\n", "Failed"),
        (false, Some(1), "", "Unavailable"),
        (false, None, "", "Unavailable"),
        (true, Some(0), "", "Unavailable"),
    ] {
        let result = ProcessResult {
            success,
            code,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
            error: None,
        };
        assert_eq!(service_query_status(&result), expected);
    }
}

#[test]
fn capture_can_be_stopped_from_overview_with_log_hidden() {
    let mut h = Harness::new([760.0, 600.0]);
    h.app.validation_task = Some(ValidationTask {
        kind: ValidationKind::Trace,
        cancel_requested: false,
        process: CaptureTask::Direct(ProcessTask::spawn("/bin/sleep", &["10".to_owned()])),
        calibration: None,
    });
    h.settle();
    h.click("Stop capture", 0);
    assert!(h.app.validation_task.as_ref().unwrap().cancel_requested);
    assert_eq!(h.app.status, "Stopping capture…");
}

#[test]
fn cancelled_calibration_releases_ownership_without_applying_values() {
    let mut h = Harness::new([760.0, 600.0]);
    let before = h.app.model.config.clone();
    let ownership = CalibrationOwnership {
        transaction: h.app.model.begin_calibration().unwrap(),
        device: String::new(),
    };
    h.app.validation_task = Some(ValidationTask {
        kind: ValidationKind::Calibration,
        cancel_requested: true,
        process: CaptureTask::Direct(ProcessTask::spawn_capturing_stdout(
            "/bin/sh",
            &[
                "-c".to_owned(),
                "printf 'aim-accel-zero-x=101\n'".to_owned(),
            ],
        )),
        calibration: Some(ownership),
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while h.app.validation_task.is_some() && std::time::Instant::now() < deadline {
        h.app.poll_validation();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(h.app.validation_task.is_none());
    assert_eq!(h.app.status, "Capture stopped");
    assert_eq!(h.app.model.config, before);
    assert!(!h.app.model.dirty);
    assert!(h.app.model.begin_calibration().is_some());
}
