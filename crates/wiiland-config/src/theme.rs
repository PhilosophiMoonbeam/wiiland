//! Opaque pearl and dusk surfaces, evergreen typography, and purposeful sea-glass accents.
use eframe::egui::{self, Color32, CornerRadius, FontId, Frame, Margin, RichText, Stroke};

pub const ICON: &[u8] = include_bytes!("../../../res/wiiland-icon.png");

#[derive(Clone, Copy)]
pub struct Palette {
    pub canvas: Color32,
    pub surface: Color32,
    pub mist: Color32,
    pub ink: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub border: Color32,
    pub warning: Color32,
}

impl Palette {
    pub fn for_ui(ui: &egui::Ui) -> Self {
        Self::new(ui.visuals().dark_mode)
    }

    pub fn new(dark: bool) -> Self {
        if dark {
            Self {
                canvas: Color32::from_rgb(23, 30, 34),
                surface: Color32::from_rgb(33, 43, 48),
                mist: Color32::from_rgb(48, 61, 64),
                ink: Color32::from_rgb(232, 238, 234),
                muted: Color32::from_rgb(170, 187, 181),
                accent: Color32::from_rgb(135, 207, 185),
                on_accent: Color32::from_rgb(18, 60, 50),
                border: Color32::from_rgb(72, 91, 93),
                warning: Color32::from_rgb(236, 195, 141),
            }
        } else {
            Self {
                canvas: Color32::from_rgb(242, 241, 234),
                surface: Color32::from_rgb(253, 253, 248),
                mist: Color32::from_rgb(231, 236, 230),
                ink: Color32::from_rgb(32, 60, 56),
                muted: Color32::from_rgb(89, 105, 96),
                accent: Color32::from_rgb(23, 102, 90),
                on_accent: Color32::from_rgb(247, 252, 248),
                border: Color32::from_rgb(188, 201, 192),
                warning: Color32::from_rgb(133, 81, 23),
            }
        }
    }
}

pub fn install(ctx: &egui::Context) {
    for theme in [egui::Theme::Light, egui::Theme::Dark] {
        let dark = theme == egui::Theme::Dark;
        let p = Palette::new(dark);
        let mut style = egui::Style {
            text_styles: [
                (egui::TextStyle::Small, FontId::proportional(12.0)),
                (egui::TextStyle::Body, FontId::proportional(14.0)),
                (egui::TextStyle::Button, FontId::proportional(14.0)),
                (egui::TextStyle::Heading, FontId::proportional(19.0)),
                (egui::TextStyle::Monospace, FontId::monospace(12.0)),
            ]
            .into(),
            ..Default::default()
        };
        style.spacing.item_spacing = egui::vec2(10.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style.spacing.interact_size = egui::vec2(36.0, 32.0);
        style.spacing.combo_width = 190.0;
        style.spacing.text_edit_width = 240.0;
        let v = &mut style.visuals;
        *v = if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        v.panel_fill = p.canvas;
        v.window_fill = p.surface;
        v.extreme_bg_color = p.canvas;
        v.text_edit_bg_color = Some(p.canvas);
        v.faint_bg_color = p.mist;
        v.code_bg_color = p.mist;
        v.override_text_color = None;
        v.weak_text_color = Some(p.muted);
        v.hyperlink_color = p.accent;
        v.warn_fg_color = p.warning;
        v.error_fg_color = if dark {
            Color32::from_rgb(244, 163, 154)
        } else {
            Color32::from_rgb(166, 48, 40)
        };
        v.selection.bg_fill = p.mist;
        v.selection.stroke = Stroke::new(1.5_f32, p.accent);
        v.slider_trailing_fill = true;
        v.window_stroke = Stroke::new(1.0_f32, p.border);
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(6);
        v.text_cursor.stroke.color = p.accent;
        v.disabled_alpha = 0.65;
        v.widgets.noninteractive.bg_fill = p.surface;
        v.widgets.noninteractive.weak_bg_fill = p.canvas;
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.ink);
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.border);
        v.widgets.noninteractive.corner_radius = CornerRadius::same(6);
        for widget in [
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            widget.corner_radius = CornerRadius::same(6);
            widget.bg_fill = p.surface;
            widget.weak_bg_fill = p.surface;
            widget.bg_stroke = Stroke::new(1.0_f32, p.border);
            widget.fg_stroke = Stroke::new(1.3_f32, p.ink);
            widget.expansion = 0.0;
        }
        // egui paints slider rails and idle handles with the mandatory widget fill.
        v.widgets.inactive.bg_fill = p.border;
        v.widgets.hovered.bg_fill = p.mist;
        v.widgets.hovered.weak_bg_fill = p.mist;
        v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.accent);
        v.widgets.active.bg_fill = p.mist;
        v.widgets.active.weak_bg_fill = p.mist;
        // egui also uses the active visuals for keyboard focus.
        v.widgets.active.bg_stroke = Stroke::new(2.0_f32, p.accent);
        v.widgets.open.bg_fill = p.mist;
        v.widgets.open.weak_bg_fill = p.mist;
        v.widgets.open.bg_stroke = Stroke::new(1.5_f32, p.accent);
        ctx.set_style_of(theme, style);
    }
    ctx.options_mut(|options| options.fallback_theme = egui::Theme::Light);
}

pub fn card(ui: &egui::Ui) -> Frame {
    let p = Palette::for_ui(ui);
    Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(8)
        .inner_margin(16)
}

pub fn panel(fill: Color32, margin: i8) -> Frame {
    Frame::new().fill(fill).inner_margin(Margin::same(margin))
}

pub fn heading(ui: &mut egui::Ui, title: &str, description: &str) {
    let p = Palette::for_ui(ui);
    ui.label(RichText::new(title).size(28.0).strong().color(p.ink));
    note(ui, description);
    ui.add_space(8.0);
}

pub fn primary(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let p = Palette::for_ui(ui);
    let enabled = enabled && ui.is_enabled();
    let response = ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).strong().color(if enabled {
            p.on_accent
        } else {
            p.ink
        }))
        .fill(if enabled { p.accent } else { p.mist })
        .stroke(if enabled {
            Stroke::NONE
        } else {
            Stroke::new(1.0_f32, p.border)
        }),
    );
    // A custom solid fill must retain a contrasting interaction/focus indicator.
    if enabled && (response.hovered() || response.has_focus()) {
        ui.painter().rect_stroke(
            response.rect.shrink(3.0),
            CornerRadius::same(3),
            Stroke::new(
                if response.has_focus() {
                    2.0_f32
                } else {
                    1.0_f32
                },
                p.on_accent,
            ),
            egui::StrokeKind::Inside,
        );
    }
    response
}

pub fn note(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .size(13.0)
            .color(Palette::for_ui(ui).muted),
    );
}

pub fn badge(ui: &mut egui::Ui, text: &str, warning: bool) {
    let p = Palette::for_ui(ui);
    Frame::new()
        .fill(p.mist)
        .corner_radius(4)
        .inner_margin(Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(12.0).color(if warning {
                p.warning
            } else {
                p.muted
            }));
        });
}
