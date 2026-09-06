//! Deterministic per-device processing. Hardware ownership and clocks live in adapters.
use crate::aim::{AimConfig, AimResult, AimState};
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
        code: u32,
        state: u32,
    },
    Motion {
        kind: MotionKind,
        axes: [Abs3; 8],
    },
    Ir(IrFrame),
    /// One fixed 16ms pointer tick, supplied by the reactor or a replay.
    PointerTick,
    /// Outputs are destroyed by the adapter when an interface disappears.
    Reset,
}

pub struct DeviceEngine {
    config: ValidatedConfig,
    profile: Profile,
    pointer: PointerState,
    aim: AimState,
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
                self.aim.reset();
            }
            EngineInput::PointerTick => {
                if self.pointer_active() {
                    let delta = self.pointer.tick();
                    self.emit_pointer(delta.dx, delta.dy, output);
                }
            }
            EngineInput::Key { code, state } => self.key(code, state, output),
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

    fn key(&mut self, code: u32, state: u32, output: &mut Vec<OutputAction>) {
        if state > 2 {
            return;
        }
        if needs_gamepad(self.profile, self.config.get())
            && let Some(mapped) = mapping::map_key(code)
        {
            output.push(OutputAction::Key(OutputDevice::Gamepad, mapped, state));
        }
        if self.profile.contains(Profile::DESKTOP) {
            let bindings = &self.config.get().desktop_bindings;
            let action = match code {
                4 => bindings.a,
                5 => bindings.b,
                6 => bindings.plus,
                7 => bindings.minus,
                8 => bindings.home,
                9 => bindings.one,
                10 => bindings.two,
                _ => DesktopAction::Disabled,
            };
            let mapped = match action {
                DesktopAction::LeftClick => Some(0x110),
                DesktopAction::RightClick => Some(0x111),
                DesktopAction::Enter => Some(28),
                DesktopAction::Escape => Some(1),
                DesktopAction::Overview => Some(125),
                DesktopAction::PageUp => Some(104),
                DesktopAction::PageDown => Some(109),
                DesktopAction::Disabled => None,
            };
            if let Some(mapped) = mapped {
                output.push(OutputAction::Key(OutputDevice::Desktop, mapped, state));
            }
            let bit = match code {
                0 => POINTER_LEFT,
                1 => POINTER_RIGHT,
                2 => POINTER_UP,
                3 => POINTER_DOWN,
                _ => 0,
            };
            if bit != 0 {
                let delta = self.pointer.update_key(bit, state != 0);
                self.emit_pointer(delta.dx, delta.dy, output);
            }
        }
        let result = self.aim.activation_key(code, state != 0);
        self.emit_aim(result, output);
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

pub fn needs_gamepad(profile: Profile, config: &Config) -> bool {
    profile.contains(Profile::GAMEPAD) || config.aim_mode == AimMode::RightStick
}
pub fn needs_desktop(profile: Profile, config: &Config) -> bool {
    profile.contains(Profile::DESKTOP) || config.aim_mode == AimMode::Mouse
}
