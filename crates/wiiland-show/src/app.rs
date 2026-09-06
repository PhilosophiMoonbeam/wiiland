use std::collections::VecDeque;
use std::io;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use wiiland_hid::{
    Axis3, Button, ButtonEvent, ButtonState, Event, EventKind, EventType, Interface, InterfaceMask,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewMode {
    Error,
    Basic,
    Extended,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Continue,
    Quit,
}

/// All values shown by wiiland-show live here.  The reducer is intentionally independent
/// of ratatui, making event handling deterministic and display-neutral.
pub struct App {
    pub mode: ViewMode,
    pub frozen: bool,
    pub gone: bool,
    pub keys_enabled: bool,
    pub accel_enabled: bool,
    pub ir_enabled: bool,
    pub motion_plus_enabled: bool,
    pub nunchuk_enabled: bool,
    pub classic_enabled: bool,
    pub balance_enabled: bool,
    pub pro_enabled: bool,
    pub drums_enabled: bool,
    pub guitar_enabled: bool,
    pub available: InterfaceMask,
    pub opened: InterfaceMask,
    pub rumble_writable: bool,
    pub rumble: bool,
    pub led_writable: [bool; 4],
    pub leds: [bool; 4],
    pub battery: Option<u8>,
    pub device_type: String,
    pub extension: String,
    pub accel: Axis3,
    pub ir: [Axis3; 4],
    pub motion_plus: Axis3,
    pub nunchuk: [Axis3; 2],
    pub classic: [Axis3; 3],
    pub balance: [Axis3; 4],
    pub pro: [Axis3; 2],
    pub drums: [Axis3; 8],
    pub guitar: [Axis3; 3],
    pub key_state: [bool; 28],
    pub nunchuk_keys: [bool; 28],
    pub classic_keys: [bool; 28],
    pub pro_keys: [bool; 28],
    pub drums_keys: [bool; 28],
    pub guitar_keys: [bool; 28],
    pub status: VecDeque<String>,
    pub last_event: Option<EventType>,
    mp_pending_calibration: bool,
    pub mp_position: [i32; 2],
}

impl Default for App {
    fn default() -> Self {
        let mut ir = [Axis3::default(); 4];
        for point in &mut ir {
            point.x = 1023;
            point.y = 1023;
        }
        Self {
            mode: ViewMode::Error,
            frozen: false,
            gone: false,
            keys_enabled: false,
            accel_enabled: false,
            ir_enabled: false,
            motion_plus_enabled: false,
            nunchuk_enabled: false,
            classic_enabled: false,
            balance_enabled: false,
            pro_enabled: false,
            drums_enabled: false,
            guitar_enabled: false,
            available: InterfaceMask::empty(),
            opened: InterfaceMask::empty(),
            rumble_writable: false,
            rumble: false,
            led_writable: [true; 4],
            leds: [false; 4],
            battery: None,
            device_type: String::from("N/A"),
            extension: String::from("N/A"),
            accel: Axis3::default(),
            ir,
            motion_plus: Axis3::default(),
            nunchuk: [Axis3::default(); 2],
            classic: [Axis3::default(); 3],
            balance: [Axis3::default(); 4],
            pro: [Axis3::default(); 2],
            drums: [Axis3::default(); 8],
            guitar: [Axis3::default(); 3],
            key_state: [false; 28],
            nunchuk_keys: [false; 28],
            classic_keys: [false; 28],
            pro_keys: [false; 28],
            drums_keys: [false; 28],
            guitar_keys: [false; 28],
            status: VecDeque::with_capacity(8),
            last_event: None,
            mp_pending_calibration: false,
            mp_position: [5000, 5000],
        }
    }
}

impl App {
    pub fn resize(&mut self, width: u16, height: u16) {
        self.mode = if width < 80 || height < 24 {
            ViewMode::Error
        } else if width < 160 || height < 48 {
            ViewMode::Basic
        } else {
            ViewMode::Extended
        };
    }

    pub fn info(&mut self, message: impl Into<String>) {
        self.push_status(message.into(), false);
    }
    pub fn error(&mut self, message: impl Into<String>) {
        self.push_status(message.into(), true);
    }
    fn push_status(&mut self, message: String, is_error: bool) {
        let prefix = if is_error { "Error: " } else { "" };
        self.status.push_back(format!("{prefix}{message}"));
        while self.status.len() > 8 {
            self.status.pop_front();
        }
    }

    pub fn apply_event(&mut self, event: &Event) -> bool {
        self.last_event = Some(event_type(event.kind));
        match event.kind {
            EventKind::Watch => {
                self.info("Watch event");
                return true;
            }
            EventKind::Gone => {
                self.gone = true;
                self.opened = InterfaceMask::empty();
                self.info("Device gone");
                return false;
            }
            _ if self.frozen => return false,
            EventKind::Key(key) => set_key(&mut self.key_state, key),
            EventKind::Accel(value) => self.accel = value,
            EventKind::Ir(values) => self.ir = values,
            EventKind::MotionPlus(value) => {
                self.motion_plus = value;
                self.mp_position[0] = (self.mp_position[0] + value.x / 100).clamp(0, 10_000);
                self.mp_position[1] = (self.mp_position[1] + value.z / 100).clamp(0, 10_000);
            }
            EventKind::NunchukKey(key) => set_key(&mut self.nunchuk_keys, key),
            EventKind::NunchukMove(values) => self.nunchuk = values,
            EventKind::ClassicControllerKey(key) => set_key(&mut self.classic_keys, key),
            EventKind::ClassicControllerMove(values) => self.classic = values,
            EventKind::BalanceBoard(values) => self.balance = values,
            EventKind::ProControllerKey(key) => set_key(&mut self.pro_keys, key),
            EventKind::ProControllerMove(values) => self.pro = values,
            EventKind::DrumsKey(key) => set_key(&mut self.drums_keys, key),
            EventKind::DrumsMove(values) => self.drums = values,
            EventKind::GuitarKey(key) => set_key(&mut self.guitar_keys, key),
            EventKind::GuitarMove(values) => self.guitar = values,
            _ => {}
        }
        false
    }

    fn handle_local_command(&mut self, ch: char) -> Option<Action> {
        match ch {
            'q' => Some(Action::Quit),
            'f' => {
                self.frozen = !self.frozen;
                self.info(if self.frozen {
                    "Freeze screen"
                } else {
                    "Unfreeze screen"
                });
                Some(Action::Continue)
            }
            's' => {
                self.mp_pending_calibration = true;
                self.info("Keep Motion Plus flat and still for recalibration");
                Some(Action::Continue)
            }
            _ => None,
        }
    }

    fn finish_mp_calibration(
        &mut self,
        sample: Axis3,
        current: ([i32; 3], i32),
    ) -> Option<([i32; 3], i32)> {
        if !self.mp_pending_calibration
            || sample.x.unsigned_abs() >= 5000
            || sample.y.unsigned_abs() >= 5000
            || sample.z.unsigned_abs() >= 5000
        {
            return None;
        }
        let ([x, y, z], factor) = current;
        let offsets = [
            x.saturating_add(sample.x),
            y.saturating_add(sample.y),
            z.saturating_add(sample.z),
        ];
        self.mp_pending_calibration = false;
        self.info(format!(
            "Calibrate MP Norm: ({}:{}:{})",
            offsets[0], offsets[1], offsets[2]
        ));
        Some((offsets, factor))
    }

    pub fn handle_key(&mut self, key: KeyEvent, iface: &mut Interface) -> io::Result<Action> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(Action::Continue);
        }
        let KeyCode::Char(ch) = key.code else {
            return Ok(Action::Continue);
        };
        if ch == 's' {
            self.refresh_static(iface);
        }
        if let Some(action) = self.handle_local_command(ch) {
            return Ok(action);
        }
        match ch {
            'k' => self.toggle_interface(iface, InterfaceMask::CORE, "key events", true),
            'a' => self.toggle_interface(iface, InterfaceMask::ACCEL, "accelerometer", false),
            'i' => self.toggle_interface(iface, InterfaceMask::IR, "IR", false),
            'm' => self.toggle_interface(iface, InterfaceMask::MOTION_PLUS, "Motion Plus", false),
            'n' => {
                let ([x, y, z], factor) = iface.mp_normalization();
                if factor == 0 {
                    iface.set_mp_normalization(x, y, z, 50);
                    self.info(format!("Enable MP Norm: ({x}:{y}:{z})"));
                } else {
                    iface.set_mp_normalization(x, y, z, 0);
                    self.info(format!("Disable MP Norm: ({x}:{y}:{z})"));
                }
            }
            'N' => self.toggle_interface(iface, InterfaceMask::NUNCHUK, "Nunchuk", false),
            'c' => self.toggle_interface(
                iface,
                InterfaceMask::CLASSIC_CONTROLLER,
                "Classic Controller",
                false,
            ),
            'b' => {
                self.toggle_interface(iface, InterfaceMask::BALANCE_BOARD, "Balance Board", false)
            }
            'p' => self.toggle_interface(
                iface,
                InterfaceMask::PRO_CONTROLLER,
                "Pro Controller",
                false,
            ),
            'g' => self.toggle_interface(iface, InterfaceMask::GUITAR, "Guitar controller", false),
            'd' => self.toggle_interface(iface, InterfaceMask::DRUMS, "Drums controller", false),
            'r' => self.toggle_rumble(iface),
            '1'..='4' => self.toggle_led(iface, (ch as usize) - ('1' as usize)),
            _ => {}
        }
        Ok(Action::Continue)
    }

    fn toggle_interface(
        &mut self,
        iface: &mut Interface,
        bit: InterfaceMask,
        label: &str,
        writable: bool,
    ) {
        if iface.opened().contains(bit) {
            iface.close(bit);
            self.opened = iface.opened();
            if bit == InterfaceMask::CORE {
                self.keys_enabled = false;
                self.rumble_writable = self.opened.contains(InterfaceMask::PRO_CONTROLLER);
            }
            self.info(format!("Disable {label}"));
            return;
        }
        let opened = match iface.open(
            bit | if writable {
                InterfaceMask::WRITABLE
            } else {
                InterfaceMask::empty()
            },
        ) {
            Ok(opened) => {
                if writable {
                    self.rumble_writable = true;
                }
                Some(opened)
            }
            Err(_) if writable => {
                self.rumble_writable = false;
                match iface.open(bit) {
                    Ok(opened) => Some(opened),
                    Err(error) => {
                        self.opened = error.opened();
                        self.error(format!("Cannot enable {label}: {error}"));
                        None
                    }
                }
            }
            Err(error) => {
                self.opened = error.opened();
                self.error(format!("Cannot enable {label}: {error}"));
                None
            }
        };
        if let Some(opened) = opened {
            self.opened = opened;
            self.set_enabled(bit, true);
            self.info(format!("Enable {label}"));
        }
    }

    fn set_enabled(&mut self, bit: InterfaceMask, value: bool) {
        match bit {
            InterfaceMask::CORE => self.keys_enabled = value,
            InterfaceMask::ACCEL => self.accel_enabled = value,
            InterfaceMask::IR => self.ir_enabled = value,
            InterfaceMask::MOTION_PLUS => self.motion_plus_enabled = value,
            InterfaceMask::NUNCHUK => self.nunchuk_enabled = value,
            InterfaceMask::CLASSIC_CONTROLLER => self.classic_enabled = value,
            InterfaceMask::BALANCE_BOARD => self.balance_enabled = value,
            InterfaceMask::PRO_CONTROLLER => self.pro_enabled = value,
            InterfaceMask::DRUMS => self.drums_enabled = value,
            InterfaceMask::GUITAR => self.guitar_enabled = value,
            _ => {}
        }
    }
    fn toggle_rumble(&mut self, iface: &mut Interface) {
        if !self.rumble_writable {
            self.error("Rumble unavailable (read-only input)");
            return;
        }
        self.rumble = !self.rumble;
        if let Err(e) = iface.rumble(self.rumble) {
            self.rumble = !self.rumble;
            self.error(format!("Cannot set rumble: {e}"));
        } else {
            self.info(if self.rumble {
                "Enable rumble"
            } else {
                "Disable rumble"
            });
        }
    }
    fn toggle_led(&mut self, iface: &mut Interface, index: usize) {
        if !self.led_writable[index] {
            self.error(format!("LED {} unavailable", index + 1));
            return;
        }
        let next = !self.leds[index];
        if let Err(e) = iface.set_led(index, next) {
            self.led_writable[index] = false;
            self.error(format!("Cannot set LED {}: {e}", index + 1));
        } else {
            self.leds[index] = next;
            self.info(format!(
                "LED {} {}",
                index + 1,
                if next { "on" } else { "off" }
            ));
        }
    }

    pub fn open_available(&mut self, iface: &mut Interface) {
        self.available = iface.available();
        let controls = InterfaceMask::CORE | InterfaceMask::PRO_CONTROLLER;
        if (iface.opened() & controls).is_empty() {
            self.rumble_writable = false;
        }
        for bit in [InterfaceMask::CORE, InterfaceMask::PRO_CONTROLLER] {
            if self.available.contains(bit) && !iface.opened().contains(bit) {
                match iface.open(bit | InterfaceMask::WRITABLE) {
                    Ok(_) => self.rumble_writable = true,
                    Err(error) => self.opened = error.opened(),
                }
            }
        }
        if let Err(error) = iface.open(self.available & InterfaceMask::ALL) {
            let missing = self.available & InterfaceMask::ALL & !iface.opened();
            if !missing.is_empty() {
                self.error(format!("Cannot open some readable interfaces: {error}"));
            }
        }
        self.opened = iface.opened();
        for bit in [
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
        ] {
            self.set_enabled(bit, self.opened.contains(bit));
        }
        if !(self.available & controls).is_empty() && !self.rumble_writable {
            self.info("Read-only input; rumble is unavailable");
        }
        self.refresh_static(iface);
    }

    fn refresh_static(&mut self, iface: &Interface) {
        self.battery = iface.battery().ok();
        for index in 0..self.leds.len() {
            match iface.get_led(index) {
                Ok(state) => {
                    self.leds[index] = state;
                    self.led_writable[index] = true;
                }
                Err(_) => self.led_writable[index] = false,
            }
        }
        self.device_type = read_attr(iface, "devtype");
        self.extension = read_attr(iface, "extension");
        self.info("Refresh static values");
    }
}

fn read_attr(iface: &Interface, name: &str) -> String {
    iface
        .attr(name)
        .ok()
        .and_then(|v| String::from_utf8(v).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from("N/A"))
}

fn event_type(kind: EventKind) -> EventType {
    kind.event_type()
}

fn set_key(keys: &mut [bool; 28], key: ButtonEvent) {
    let Some(index) = button_index(key.button) else {
        return;
    };
    let pressed = match key.state {
        ButtonState::Pressed | ButtonState::Repeated => true,
        ButtonState::Released => false,
        _ => return,
    };
    keys[index] = pressed;
}

fn button_index(button: Button) -> Option<usize> {
    Some(button.code() as usize)
}

pub(crate) const BUTTON_ORDER: [Button; 28] = Button::ALL;

pub(crate) fn button_at(index: usize) -> Option<Button> {
    BUTTON_ORDER.get(index).copied()
}

pub fn key_name(button: Button) -> &'static str {
    match button {
        Button::Left => "LEFT",
        Button::Right => "RIGHT",
        Button::Up => "UP",
        Button::Down => "DOWN",
        Button::A => "A",
        Button::B => "B",
        Button::Plus => "PLUS",
        Button::Minus => "MINUS",
        Button::Home => "HOME",
        Button::One => "1",
        Button::Two => "2",
        Button::X => "X",
        Button::Y => "Y",
        Button::ShoulderLeft => "TL",
        Button::ShoulderRight => "TR",
        Button::TriggerLeft => "ZL",
        Button::TriggerRight => "ZR",
        Button::ThumbLeft => "THUMBL",
        Button::ThumbRight => "THUMBR",
        Button::C => "C",
        Button::Z => "Z",
        Button::StrumBarUp => "STRUM_UP",
        Button::StrumBarDown => "STRUM_DOWN",
        Button::FretFarUp => "FRET_FAR_UP",
        Button::FretUp => "FRET_UP",
        Button::FretMid => "FRET_MID",
        Button::FretLow => "FRET_LOW",
        Button::FretFarLow => "FRET_FAR_LOW",
        _ => "UNKNOWN",
    }
}

pub fn parse_selector(arg: &str) -> Result<Selector<'_>, &'static str> {
    if let Some(path) = arg.strip_prefix("/sys/")
        && !path.is_empty()
    {
        return Ok(Selector::Path(Path::new(arg)));
    }
    if arg.is_empty() || !arg.bytes().all(|b| b.is_ascii_digit()) {
        return Err("selector must be a positive ordinal or an absolute /sys path");
    }
    let value = arg
        .parse::<usize>()
        .map_err(|_| "selector ordinal is out of range")?;
    if value == 0 {
        return Err("selector must be a positive ordinal or an absolute /sys path");
    }
    Ok(Selector::Ordinal(value))
}
pub enum Selector<'a> {
    Ordinal(usize),
    Path(&'a Path),
}

pub fn poll_interface(iface: &mut Interface, app: &mut App) -> io::Result<bool> {
    match iface.dispatch() {
        Ok(event) => {
            let reopen = app.apply_event(&event);
            if let EventKind::MotionPlus(sample) = event.kind
                && let Some((offsets, factor)) =
                    app.finish_mp_calibration(sample, iface.mp_normalization())
            {
                iface.set_mp_normalization(offsets[0], offsets[1], offsets[2], factor);
            }
            if reopen {
                app.open_available(iface);
            }
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: EventKind) -> Event {
        Event {
            time: wiiland_hid::Timestamp {
                seconds: 0,
                microseconds: 0,
            },
            kind,
        }
    }

    #[test]
    fn frozen_values_stay_fixed_while_lifecycle_events_request_reopen_and_mark_gone() {
        let mut app = App {
            frozen: true,
            opened: InterfaceMask::CORE | InterfaceMask::ACCEL,
            accel: Axis3 { x: 1, y: 2, z: 3 },
            ..App::default()
        };
        app.key_state[4] = true;

        assert!(!app.apply_event(&event(EventKind::Accel(Axis3 {
            x: 10,
            y: 20,
            z: 30,
        }))));
        let key = event(EventKind::Key(ButtonEvent {
            button: Button::A,
            state: ButtonState::Released,
        }));
        assert!(!app.apply_event(&key));
        assert_eq!(app.accel, Axis3 { x: 1, y: 2, z: 3 });
        assert!(app.key_state[4]);

        assert!(app.apply_event(&event(EventKind::Watch)));
        assert_eq!(app.status.back().map(String::as_str), Some("Watch event"));

        assert!(!app.apply_event(&event(EventKind::Gone)));
        assert!(app.gone);
        assert!(app.opened.is_empty());
        assert_eq!(app.status.back().map(String::as_str), Some("Device gone"));
    }

    #[test]
    fn known_button_states_preserve_display_behavior() {
        let mut keys = [false; 28];

        set_key(
            &mut keys,
            ButtonEvent {
                button: Button::A,
                state: ButtonState::Pressed,
            },
        );
        assert!(keys[4]);

        keys[4] = false;
        set_key(
            &mut keys,
            ButtonEvent {
                button: Button::A,
                state: ButtonState::Repeated,
            },
        );
        assert!(keys[4]);

        set_key(
            &mut keys,
            ButtonEvent {
                button: Button::A,
                state: ButtonState::Released,
            },
        );
        assert!(!keys[4]);
    }

    #[test]
    fn typed_button_names_preserve_labels_and_order() {
        assert_eq!(key_name(Button::A), "A");
        assert_eq!(key_name(Button::ShoulderLeft), "TL");
        assert_eq!(key_name(Button::FretFarLow), "FRET_FAR_LOW");
        assert_eq!(button_at(27).map(key_name), Some("FRET_FAR_LOW"));
        assert_eq!(button_at(28), None);
    }

    #[test]
    fn s_starts_flat_calibration_and_stable_sample_preserves_normalization_factor() {
        let mut app = App {
            frozen: true,
            motion_plus: Axis3 {
                x: 91,
                y: 92,
                z: 93,
            },
            ..App::default()
        };

        assert_eq!(app.handle_local_command('s'), Some(Action::Continue));
        assert!(app.mp_pending_calibration);
        let sample = Axis3 {
            x: 120,
            y: -80,
            z: 40,
        };
        assert_eq!(
            app.finish_mp_calibration(sample, ([1000, -2000, 3000], 50)),
            Some(([1120, -2080, 3040], 50))
        );
        assert!(!app.mp_pending_calibration);

        assert_eq!(app.handle_local_command('s'), Some(Action::Continue));
        assert_eq!(
            app.finish_mp_calibration(Axis3 { x: 1, y: 2, z: 3 }, ([1120, -2080, 3040], 0)),
            Some(([1121, -2078, 3043], 0))
        );
    }

    #[test]
    fn flat_calibration_rejects_implausible_sample_without_clearing_request() {
        let mut app = App::default();
        assert_eq!(app.handle_local_command('s'), Some(Action::Continue));

        assert_eq!(
            app.finish_mp_calibration(
                Axis3 {
                    x: 4999,
                    y: -5000,
                    z: 0
                },
                ([10, 20, 30], 50)
            ),
            None
        );
        assert!(app.mp_pending_calibration);
        assert_eq!(
            app.finish_mp_calibration(
                Axis3 {
                    x: i32::MIN,
                    y: 0,
                    z: 0
                },
                ([10, 20, 30], 50)
            ),
            None
        );
        assert!(app.mp_pending_calibration);
    }
}
