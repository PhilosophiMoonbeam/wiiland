//! Deterministic per-device processing. Hardware ownership and clocks live in adapters.
use crate::aim::{AimConfig, AimResult, AimState};
use crate::input::{Button, ButtonState, InputSource};
use crate::mapping::{self, Abs3, MotionKind};
use crate::pointer::{
    IrFrame, POINTER_DOWN, POINTER_LEFT, POINTER_RIGHT, POINTER_UP, PointerState,
};
use crate::{AimMode, Config, DesktopAction, Profile, ValidatedConfig};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputDevice {
    Gamepad,
    Desktop,
}

/// Key actions include their synchronization boundary; motion uses explicit Sync.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputAction {
    Key(OutputDevice, u16, u32),
    Abs(OutputDevice, u16, i32),
    Rel(OutputDevice, u16, i32),
    Sync(OutputDevice),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineInput {
    Key {
        source: InputSource,
        button: Button,
        state: ButtonState,
    },
    Motion {
        kind: MotionKind,
        axes: [Abs3; 8],
    },
    Ir(IrFrame),
    /// One fixed 16ms pointer tick, supplied by the reactor or a replay.
    PointerTick,
    /// Release only this interface's held buttons while retaining other sources.
    SourceRemoved(InputSource),
    /// Start a new session after the adapter destroys the previous outputs.
    Reset,
}

pub struct DeviceEngine {
    config: ValidatedConfig,
    profile: Profile,
    pointer: PointerState,
    aim: AimState,
    held: [u32; InputSource::ALL.len()],
}

impl DeviceEngine {
    pub fn new(config: ValidatedConfig, profile: Profile) -> Self {
        let c = config.get();
        Self {
            profile,
            pointer: PointerState::new(
                c.pointer_speed,
                c.ir_speed,
                c.ir_deadzone,
                c.ir_smoothing,
                c.ir_tracking,
            ),
            aim: AimState::new(AimConfig::from_config(c)),
            config,
            held: [0; InputSource::ALL.len()],
        }
    }

    pub fn pointer_active(&self) -> bool {
        self.pointer.pointer_keys() != 0
    }

    /// Appends ordered actions to a caller-owned buffer, reusable between reports.
    pub fn process(&mut self, input: EngineInput, output: &mut Vec<OutputAction>) {
        match input {
            EngineInput::Reset => {
                self.pointer.reset();
                self.aim.reset_session();
                self.held.fill(0);
            }
            EngineInput::PointerTick => {
                if self.pointer_active() {
                    let delta = self.pointer.tick();
                    self.emit_pointer(delta.dx, delta.dy, output);
                }
            }
            EngineInput::Key {
                source,
                button,
                state,
            } => self.key(source, button, state, output),
            EngineInput::SourceRemoved(source) => {
                let held = self.held[source.index()];
                for button in Button::ALL {
                    if held & button_bit(button) != 0 {
                        self.key(source, button, ButtonState::Released, output);
                    }
                }
            }
            EngineInput::Motion { kind, axes } => {
                if needs_gamepad(self.profile, self.config.get()) {
                    let mapped = mapping::map_motion(kind, axes);
                    for axis in mapped.axes.iter().take(mapped.count) {
                        output.push(OutputAction::Abs(
                            OutputDevice::Gamepad,
                            axis.code,
                            axis.value,
                        ));
                    }
                    output.push(OutputAction::Sync(OutputDevice::Gamepad));
                }
                let v = axes[0];
                let aim = match kind {
                    MotionKind::Accel => self.aim.process_accelerometer([v.x, v.y, v.z]),
                    MotionKind::MotionPlus => self.aim.process_motion_plus([v.x, v.y, v.z]),
                    _ => return,
                };
                self.emit_aim(aim, output);
            }
            EngineInput::Ir(frame) => {
                let point = self.pointer.select_ir(&frame);
                if self.profile.contains(Profile::DESKTOP) {
                    let delta = self.pointer.update_ir_frame(&frame);
                    self.emit_pointer(delta.dx, delta.dy, output);
                }
                let result = self.aim.process_ir(point);
                self.emit_aim(result, output);
            }
        }
    }

    fn held_buttons(&self) -> u32 {
        self.held.iter().fold(0, |held, source| held | source)
    }

    fn desktop_key(&self, button: Button) -> Option<u16> {
        let bindings = &self.config.get().desktop_bindings;
        let action = match button {
            Button::A => bindings.a,
            Button::B => bindings.b,
            Button::Plus => bindings.plus,
            Button::Minus => bindings.minus,
            Button::Home => bindings.home,
            Button::One => bindings.one,
            Button::Two => bindings.two,
            _ => DesktopAction::Disabled,
        };
        match action {
            DesktopAction::LeftClick => Some(0x110),
            DesktopAction::RightClick => Some(0x111),
            DesktopAction::Enter => Some(28),
            DesktopAction::Escape => Some(1),
            DesktopAction::Overview => Some(125),
            DesktopAction::PageUp => Some(104),
            DesktopAction::PageDown => Some(109),
            DesktopAction::Disabled => None,
        }
    }

    fn desktop_key_held(&self, held: u32, key: u16) -> bool {
        Button::ALL
            .into_iter()
            .any(|button| held & button_bit(button) != 0 && self.desktop_key(button) == Some(key))
    }

    fn key(
        &mut self,
        source: InputSource,
        button: Button,
        state: ButtonState,
        output: &mut Vec<OutputAction>,
    ) {
        let before = self.held_buttons();
        let bit = button_bit(button);
        match state {
            ButtonState::Released => self.held[source.index()] &= !bit,
            ButtonState::Pressed | ButtonState::Repeated => self.held[source.index()] |= bit,
        }
        let after = self.held_buttons();
        let was_held = before & bit != 0;
        let is_held = after & bit != 0;
        if needs_gamepad(self.profile, self.config.get()) {
            let mapped = mapping::map_button(button);
            emit_key_transition(
                OutputDevice::Gamepad,
                mapped,
                was_held,
                is_held,
                state,
                output,
            );
        }
        if self.profile.contains(Profile::DESKTOP) {
            if let Some(mapped) = self.desktop_key(button) {
                emit_key_transition(
                    OutputDevice::Desktop,
                    mapped,
                    self.desktop_key_held(before, mapped),
                    self.desktop_key_held(after, mapped),
                    state,
                    output,
                );
            }
            let pointer_bit = match button {
                Button::Left => POINTER_LEFT,
                Button::Right => POINTER_RIGHT,
                Button::Up => POINTER_UP,
                Button::Down => POINTER_DOWN,
                _ => 0,
            };
            if pointer_bit != 0 && was_held != is_held {
                let delta = self.pointer.update_key(pointer_bit, is_held);
                self.emit_pointer(delta.dx, delta.dy, output);
            }
        }
        if was_held != is_held {
            let result = self.aim.activation_key(button.code(), is_held);
            self.emit_aim(result, output);
        }
    }

    fn emit_pointer(&self, dx: i32, dy: i32, output: &mut Vec<OutputAction>) {
        output.push(OutputAction::Rel(OutputDevice::Desktop, 0, dx));
        output.push(OutputAction::Rel(OutputDevice::Desktop, 1, dy));
        if dx != 0 || dy != 0 {
            output.push(OutputAction::Sync(OutputDevice::Desktop));
        }
    }

    fn emit_aim(&self, result: AimResult, output: &mut Vec<OutputAction>) {
        let Some(v) = result.output else {
            return;
        };
        match self.aim.config.output {
            AimMode::Mouse => self.emit_pointer(v.x, v.y, output),
            AimMode::RightStick => {
                output.push(OutputAction::Abs(
                    OutputDevice::Gamepad,
                    mapping::ABS_RX,
                    v.x.clamp(-32768, 32767),
                ));
                output.push(OutputAction::Abs(
                    OutputDevice::Gamepad,
                    mapping::ABS_RY,
                    v.y.clamp(-32768, 32767),
                ));
                output.push(OutputAction::Sync(OutputDevice::Gamepad));
            }
            AimMode::Off => {}
        }
    }
}

fn button_bit(button: Button) -> u32 {
    1 << button.code()
}

fn emit_key_transition(
    target: OutputDevice,
    code: u16,
    before: bool,
    after: bool,
    state: ButtonState,
    output: &mut Vec<OutputAction>,
) {
    let state = match (before, after) {
        (false, true) => ButtonState::Pressed,
        (true, false) => ButtonState::Released,
        (true, true) if state == ButtonState::Repeated => ButtonState::Repeated,
        _ => return,
    };
    output.push(OutputAction::Key(target, code, state.value()));
}

pub fn needs_gamepad(profile: Profile, config: &Config) -> bool {
    profile.contains(Profile::GAMEPAD) || config.aim_mode == AimMode::RightStick
}
pub fn needs_desktop(profile: Profile, config: &Config) -> bool {
    profile.contains(Profile::DESKTOP) || config.aim_mode == AimMode::Mouse
}
