use eframe::egui;
use wiiland_core::{
    AimActivation, AimMode, AimSource, DeviceRuleKind, IrAimMapping, IrTracking, Profile,
    SensorCalibration,
};

use crate::{
    model::{self, ConfigModel},
    theme,
};

pub(super) fn draw_profile(ui: &mut egui::Ui, model: &mut ConfigModel) {
    let mut changed = false;
    theme::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        group_heading(
            ui,
            "Default profile",
            "Choose the output for controllers without a matching device rule.",
        );
        changed |= choice_cards(
            ui,
            &mut model.config.profile,
            [
                (Profile::GAMEPAD, "Gamepad", "Games & emulators"),
                (Profile::DESKTOP, "Desktop", "Pointer & keyboard"),
                (Profile::BOTH, "Both", "Both outputs available"),
            ],
        );
    });
    ui.add_space(12.0);
    theme::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        group_heading(
            ui,
            "Pointer response",
            "Desktop and Both profiles. Drag a slider or click its number to enter an exact value.",
        );
        changed |= slider(
            ui,
            "D-pad pointer speed",
            &mut model.config.pointer_speed,
            1,
            127,
        );
        changed |= slider(ui, "IR pointer gain", &mut model.config.ir_speed, 1, 127);
        changed |= slider(
            ui,
            "IR jitter deadzone",
            &mut model.config.ir_deadzone,
            0,
            127,
        );
        changed |= slider(ui, "IR smoothing %", &mut model.config.ir_smoothing, 0, 95);
        theme::note(
            ui,
            "More smoothing steadies the pointer, with a little more delay.",
        );
        ui.add_space(10.0);
        ui.separator();
        egui::CollapsingHeader::new("Advanced IR mapping & screen calibration")
            .id_salt("ir-advanced")
            .show(ui, |ui| {
                changed |= combo_enum(
                    ui,
                    "IR tracking",
                    &mut model.config.ir_tracking,
                    [
                        (IrTracking::Dual, "Sensor-bar pair"),
                        (IrTracking::Centroid, "Visible-point centroid"),
                        (IrTracking::First, "First visible point"),
                    ],
                );
                changed |= combo_enum(
                    ui,
                    "IR aim mapping",
                    &mut model.config.ir_aim_mapping,
                    [
                        (IrAimMapping::Relative, "Relative movement"),
                        (IrAimMapping::Absolute, "Absolute screen position"),
                    ],
                );
                theme::note(
                    ui,
                    "Screen bounds map the IR sensor to an absolute screen position.",
                );
                let mut enabled = model.config.ir_screen.is_some();
                if ui
                    .checkbox(&mut enabled, "Use screen calibration")
                    .changed()
                {
                    model.config.ir_screen = enabled.then(model::ir_rect_default);
                    changed = true;
                }
                if let Some(rect) = model.config.ir_screen.as_mut() {
                    changed |= drag(ui, "IR screen left", &mut rect.left, 0, 32767);
                    changed |= drag(ui, "IR screen right", &mut rect.right, 0, 32767);
                    changed |= drag(ui, "IR screen top", &mut rect.top, 0, 32767);
                    changed |= drag(ui, "IR screen bottom", &mut rect.bottom, 0, 32767);
                    if rect.right <= rect.left || rect.bottom <= rect.top {
                        ui.colored_label(
                            theme::Palette::for_ui(ui).warning,
                            "Right must be greater than left; bottom must be greater than top.",
                        );
                    }
                }
            });
    });
    if changed {
        model.mark_dirty();
    }
}

pub(super) fn draw_aim(ui: &mut egui::Ui, model: &mut ConfigModel) {
    let mut changed = false;
    theme::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        group_heading(ui, "Motion output", "Turn controller movement into a stick or pointer, independently of the default profile.");
        changed |= choice_cards(ui, &mut model.config.aim_mode, [
            (AimMode::Off, "Off", "No motion output"),
            (AimMode::RightStick, "Right stick", "Aim in games"),
            (AimMode::Mouse, "Mouse pointer", "Move the pointer"),
        ]);
        ui.add_space(8.0);
        theme::note(ui, match model.config.aim_mode {
            AimMode::Off => "Motion is off. Your response and calibration settings are kept for next time.",
            AimMode::RightStick => "Motion drives the gamepad right stick, even with the Desktop profile.",
            AimMode::Mouse => "Motion drives the mouse pointer, even with the Gamepad profile.",
        });
    });
    ui.add_space(12.0);
    theme::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        group_heading(ui, "Response", "Tune how movement feels. These settings are saved even when motion is off.");
        changed |= slider(ui, "Sensitivity", &mut model.config.aim_sensitivity, 1, 127);
        changed |= slider(ui, "Deadzone", &mut model.config.aim_deadzone, 0, 32767);
        changed |= slider(ui, "Smoothing %", &mut model.config.aim_smoothing, 0, 95);
        theme::note(ui, "Deadzone ignores small movements. Smoothing trades immediacy for steadiness.");
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Sensor & activation").strong());
        changed |= combo_enum(ui, "Best available sensor", &mut model.config.aim_source, [
            (AimSource::Auto, "Automatic"),
            (AimSource::Ir, "IR sensor"),
            (AimSource::MotionPlus, "MotionPlus"),
            (AimSource::Accelerometer, "Accelerometer"),
        ]);
        changed |= combo_enum(ui, "Activation", &mut model.config.aim_activation, [
            (AimActivation::B, "B button"),
            (AimActivation::Always, "Always active"),
            (AimActivation::Z, "Nunchuk Z"),
            (AimActivation::C, "Nunchuk C"),
        ]);
        changed |= field_row(ui, "Direction", |ui, label_id| {
            let mut changed = false;
            ui.horizontal_wrapped(|ui| {
                changed |= ui.checkbox(&mut model.config.aim_invert_x, "Invert X")
                    .labelled_by(label_id).changed();
                changed |= ui.checkbox(&mut model.config.aim_invert_y, "Invert Y")
                    .labelled_by(label_id).changed();
            });
            changed
        });
        ui.add_space(10.0);
        ui.separator();
        egui::CollapsingHeader::new("Saved sensor calibration")
            .id_salt("sensor-calibration")
            .show(ui, |ui| {
                theme::note(ui, "Saved offsets compensate for sensor bias. For a fresh measurement, use Calibration.");
                changed |= drag(ui, "Calibration duration", &mut model.config.aim_calibration_duration, 1, 30);
                theme::note(ui, "Duration is in seconds. Collapsing this section keeps all saved values.");
                let mut accel = model.config.aim_accel_zero.is_some();
                if ui.checkbox(&mut accel, "Use accelerometer calibration").changed() {
                    model.config.aim_accel_zero = accel.then(model::calibration_default);
                    changed = true;
                }
                if let Some(cal) = model.config.aim_accel_zero.as_mut() {
                    changed |= calibration_fields(ui, "Accelerometer zero", cal);
                }
                ui.add_space(6.0);
                let mut motion = model.config.aim_motion_plus_bias.is_some();
                if ui.checkbox(&mut motion, "Use MotionPlus calibration").changed() {
                    model.config.aim_motion_plus_bias = motion.then(model::calibration_default);
                    changed = true;
                }
                if let Some(cal) = model.config.aim_motion_plus_bias.as_mut() {
                    changed |= calibration_fields(ui, "MotionPlus bias", cal);
                }
            });
    });
    if changed {
        model.mark_dirty();
    }
}

pub(super) fn draw_bindings(ui: &mut egui::Ui, model: &mut ConfigModel) {
    let mut changed = false;
    theme::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        group_heading(
            ui,
            "Desktop buttons",
            "Applies to Desktop and Both profiles. Gamepad buttons keep their normal mapping.",
        );
        ui.spacing_mut().item_spacing.y = 4.0;
        for (index, (name, label)) in model::binding_names().into_iter().enumerate() {
            if index != 0 {
                ui.separator();
            }
            let bindings = &mut model.config.desktop_bindings;
            let value = match name {
                "a" => &mut bindings.a,
                "b" => &mut bindings.b,
                "plus" => &mut bindings.plus,
                "minus" => &mut bindings.minus,
                "home" => &mut bindings.home,
                "one" => &mut bindings.one,
                _ => &mut bindings.two,
            };
            changed |= combo_enum(ui, label, value, model::desktop_actions());
        }
    });
    if changed {
        model.mark_dirty();
    }
}

// UI identities travel with rules so a reordered rule retains its own text field
// and popup state. The form edits rule strings in place.
#[derive(Clone, Default)]
struct RuleIds {
    rows: Vec<u64>,
    next: u64,
}

impl RuleIds {
    fn push(&mut self) {
        self.rows.push(self.next);
        self.next += 1;
    }
}

enum RuleEdit {
    Remove(usize),
    Move(usize, usize),
}

pub(super) fn draw_rules(ui: &mut egui::Ui, model: &mut ConfigModel) {
    let state_id = ui.make_persistent_id(("device-rule-identities", &model.config_path));
    let mut ids = ui
        .data_mut(|data| data.remove_temp::<RuleIds>(state_id))
        .unwrap_or_default();
    let count = model.config.device_rules.len();
    ids.rows.truncate(count);
    while ids.rows.len() < count {
        ids.push();
    }
    let mut changed = false;
    let mut edit = None;
    group_heading(
        ui,
        "Controller rules",
        "Rules are checked from top to bottom. The last matching rule wins; unmatched controllers use the default profile.",
    );
    if count == 0 {
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new("One profile for every controller").strong());
            theme::note(ui, "No overrides yet. Add a rule when a controller needs a different profile, using part of its device path or type.");
        });
    }
    for (index, rule) in model.config.device_rules.iter_mut().enumerate() {
        ui.push_id(("device-rule", ids.rows[index]), |ui| {
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(format!("Rule {}", index + 1)).strong());
                    if index + 1 == count {
                        theme::note(ui, "Last match has priority");
                    }
                });
                ui.add_space(4.0);
                changed |= combo_enum(
                    ui,
                    "Match by",
                    &mut rule.kind,
                    [
                        (DeviceRuleKind::Syspath, "Device path"),
                        (DeviceRuleKind::Devtype, "Device type"),
                    ],
                );
                changed |= field_row(ui, "Contains", |ui, label_id| {
                    ui.add(
                        egui::TextEdit::singleline(&mut rule.match_text)
                            .hint_text("Required match text")
                            .desired_width(f32::INFINITY)
                            .min_size(egui::vec2(0.0, 34.0)),
                    )
                    .labelled_by(label_id)
                    .changed()
                });
                changed |= combo_enum(
                    ui,
                    "Use profile",
                    &mut rule.profile,
                    model::profile_choices(),
                );
                if rule.match_text.trim().is_empty() {
                    ui.colored_label(
                        theme::Palette::for_ui(ui).warning,
                        "Enter part of the device path or type before saving.",
                    );
                }
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(index > 0, egui::Button::new("Move earlier"))
                        .on_hover_text(
                            "Lower priority: later matching rules can override this rule.",
                        )
                        .clicked()
                    {
                        edit = Some(RuleEdit::Move(index, index - 1));
                    }
                    if ui
                        .add_enabled(index + 1 < count, egui::Button::new("Move later"))
                        .on_hover_text(
                            "Higher priority: this rule can override earlier matching rules.",
                        )
                        .clicked()
                    {
                        edit = Some(RuleEdit::Move(index, index + 1));
                    }
                    if ui.button("Remove").clicked() {
                        edit = Some(RuleEdit::Remove(index));
                    }
                });
            });
        });
        ui.add_space(10.0);
    }
    match edit {
        Some(RuleEdit::Remove(index)) => {
            model.config.device_rules.remove(index);
            ids.rows.remove(index);
            changed = true;
        }
        Some(RuleEdit::Move(from, to)) => {
            model.config.device_rules.swap(from, to);
            ids.rows.swap(from, to);
            changed = true;
        }
        None => {}
    }
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(
                model.config.device_rules.len() < model::MAX_DEVICE_RULES,
                egui::Button::new("Add rule"),
            )
            .clicked()
        {
            model.config.device_rules.push(model::rule(
                DeviceRuleKind::Devtype,
                String::new(),
                Profile::GAMEPAD,
            ));
            ids.push();
            changed = true;
        }
        theme::note(
            ui,
            &format!(
                "{} / {} rules",
                model.config.device_rules.len(),
                model::MAX_DEVICE_RULES
            ),
        );
    });
    ui.data_mut(|data| data.insert_temp(state_id, ids));
    if changed {
        model.mark_dirty();
    }
}

fn group_heading(ui: &mut egui::Ui, title: &str, description: &str) {
    ui.label(egui::RichText::new(title).size(19.0).strong());
    theme::note(ui, description);
    ui.add_space(8.0);
}

fn choice_cards<T: Copy + Eq, const N: usize>(
    ui: &mut egui::Ui,
    value: &mut T,
    choices: [(T, &str, &str); N],
) -> bool {
    let before = *value;
    ui.columns(N, |columns| {
        for (column, (candidate, title, purpose)) in columns.iter_mut().zip(choices) {
            let width = column.available_width();
            if column
                .add_sized(
                    [width, 38.0],
                    egui::Button::new(title)
                        .selected(*value == candidate)
                        .wrap(),
                )
                .clicked()
            {
                *value = candidate;
            }
            theme::note(column, purpose);
        }
    });
    *value != before
}

pub(super) fn field_row(
    ui: &mut egui::Ui,
    label: &str,
    add: impl FnOnce(&mut egui::Ui, egui::Id) -> bool,
) -> bool {
    let width = ui.available_width().max(0.0);
    if width < 380.0 {
        return ui
            .vertical(|ui| {
                let label = ui.add(egui::Label::new(label).wrap());
                ui.allocate_ui_with_layout(
                    egui::vec2(width, 34.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.spacing_mut().combo_width = width;
                        add(ui, label.id)
                    },
                )
                .inner
            })
            .inner;
    }
    let spacing = ui.spacing().item_spacing.x;
    let control_width = (width * 0.56).min(360.0);
    let label_width = (width - control_width - spacing).max(0.0);
    ui.horizontal(|ui| {
        let label = ui
            .allocate_ui_with_layout(
                egui::vec2(label_width, 34.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_min_width(label_width);
                    ui.add(egui::Label::new(label).wrap())
                },
            )
            .inner;
        ui.allocate_ui_with_layout(
            egui::vec2(control_width, 34.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.spacing_mut().combo_width = control_width;
                add(ui, label.id)
            },
        )
        .inner
    })
    .inner
}

fn slider(ui: &mut egui::Ui, label: &str, value: &mut i32, min: i32, max: i32) -> bool {
    field_row(ui, label, |ui, label_id| {
        let number_width = 60.0;
        ui.spacing_mut().slider_width =
            (ui.available_width() - number_width - ui.spacing().item_spacing.x).max(24.0);
        let slider_changed = ui
            .add(
                egui::Slider::new(value, min..=max)
                    .show_value(false)
                    .clamping(egui::SliderClamping::Edits),
            )
            .labelled_by(label_id)
            .changed();
        let number_changed = ui
            .add_sized(
                [number_width, 34.0],
                egui::DragValue::new(value)
                    .range(min..=max)
                    .clamp_existing_to_range(false)
                    .speed(1.0),
            )
            .labelled_by(label_id)
            .changed();
        slider_changed || number_changed
    })
}

fn drag(ui: &mut egui::Ui, label: &str, value: &mut i32, min: i32, max: i32) -> bool {
    field_row(ui, label, |ui, label_id| {
        ui.add_sized(
            [ui.available_width(), 34.0],
            egui::DragValue::new(value)
                .range(min..=max)
                .clamp_existing_to_range(false)
                .speed(1.0),
        )
        .labelled_by(label_id)
        .changed()
    })
}

fn calibration_fields(ui: &mut egui::Ui, label: &str, cal: &mut SensorCalibration) -> bool {
    ui.push_id(label, |ui| {
        ui.add_space(4.0);
        ui.label(egui::RichText::new(label).strong());
        let mut changed = false;
        for (axis, value) in [("X", &mut cal.x), ("Y", &mut cal.y), ("Z", &mut cal.z)] {
            changed |= drag(ui, axis, value, -32768, 32767);
        }
        changed
    })
    .inner
}

fn combo_enum<T: Copy + Eq, const N: usize>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut T,
    choices: [(T, &str); N],
) -> bool {
    field_row(ui, label, |ui, label_id| {
        let before = *value;
        let selected = choices
            .iter()
            .find(|(candidate, _)| *candidate == *value)
            .map_or("", |(_, text)| *text);
        egui::ComboBox::from_id_salt(("enum", label))
            .selected_text(selected)
            .wrap_mode(egui::TextWrapMode::Truncate)
            .show_ui(ui, |ui| {
                for (candidate, text) in &choices {
                    ui.selectable_value(value, *candidate, *text);
                }
            })
            .response
            .labelled_by(label_id);
        *value != before
    })
}
