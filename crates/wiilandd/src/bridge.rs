//! Per-device event bridge and output lifecycle.
use crate::uinput::{Backend, VirtualDevice, VirtualKind};
use std::cell::Cell;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use wiiland_core::engine::{
    DeviceEngine, EngineInput, OutputAction, OutputDevice, needs_desktop, needs_gamepad,
};
use wiiland_core::mapping::{Abs3, MotionKind};
use wiiland_core::pointer::{IrFrame, IrPoint};
use wiiland_core::{
    AbsPayload, Config, KeyPayload, Profile, TraceEvent, TraceFilter, TracePayload,
};
use wiiland_hid::{Axis3, Button, ButtonState, Event, EventKind, Interface, InterfaceMask};

pub const MAX_EVENTS_PER_DRAIN: usize = 256;
pub const PROFILE_GAMEPAD: u8 = Profile::GAMEPAD.bits();
pub const PROFILE_DESKTOP: u8 = Profile::DESKTOP.bits();

fn io_errno(error: &io::Error) -> i32 {
    -error.raw_os_error().unwrap_or(libc::EIO)
}

fn button_code(button: Button) -> Option<u32> {
    Some(button.code())
}

fn button_state(state: ButtonState) -> Option<u32> {
    Some(state.value())
}

fn event_type_code(kind: EventKind) -> u32 {
    kind.event_type().code()
}
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BridgeAction {
    Continue,
    Gone,
}

type TraceSink = Box<dyn FnMut(&str)>;
type TraceClock = Box<dyn FnMut() -> i64>;

struct TraceContext {
    filter: TraceFilter,
    sequence: Rc<Cell<u64>>,
    clock: TraceClock,
    sink: TraceSink,
}

impl TraceContext {
    fn new<F: FnMut(&str) + 'static>(
        filter: TraceFilter,
        sequence: Rc<Cell<u64>>,
        sink: F,
    ) -> Self {
        Self::with_clock(filter, sequence, monotonic_time_us, sink)
    }

    fn with_clock<C: FnMut() -> i64 + 'static, F: FnMut(&str) + 'static>(
        filter: TraceFilter,
        sequence: Rc<Cell<u64>>,
        clock: C,
        sink: F,
    ) -> Self {
        Self {
            filter,
            sequence,
            clock: Box::new(clock),
            sink: Box::new(sink),
        }
    }

    fn emit(&mut self, syspath: &Path, event: &Event) {
        if !self.filter.matches(event_type_code(event.kind)) {
            return;
        }
        let sequence = self.sequence.get().wrapping_add(1).max(1);
        self.sequence.set(sequence);
        let monotonic_us = (self.clock)();
        let line = format_trace_line(sequence, monotonic_us, syspath, event);
        (self.sink)(&line);
    }
}

pub struct BridgeDevice<B: Backend + Clone = crate::uinput::SystemBackend> {
    pub(crate) syspath: PathBuf,
    pub(crate) profile: Profile,
    pub(crate) iface: Interface,
    pub(crate) gamepad: Option<VirtualDevice<B>>,
    pub(crate) desktop: Option<VirtualDevice<B>>,
    engine: DeviceEngine,
    actions: Vec<OutputAction>,
    pub(crate) opened_ifaces: InterfaceMask,
    pub(crate) pending_ifaces: InterfaceMask,
    trace: Option<TraceContext>,
    config: Config,
    backend: B,
    outputs_enabled: bool,
    capture: bool,
}
impl BridgeDevice<crate::uinput::SystemBackend> {
    pub fn new(path: impl AsRef<Path>, config: &Config) -> Result<Self, i32> {
        Self::with_backend(path, config, crate::uinput::SystemBackend)
    }
}
impl<B: Backend + Clone> BridgeDevice<B> {
    pub fn with_backend(path: impl AsRef<Path>, config: &Config, backend: B) -> Result<Self, i32> {
        Self::with_backend_outputs(path, config, backend, true)
    }
    pub fn with_backend_outputs(
        path: impl AsRef<Path>,
        config: &Config,
        backend: B,
        outputs_enabled: bool,
    ) -> Result<Self, i32> {
        let validated = config.clone().try_into().map_err(|_| -libc::EINVAL)?;
        let iface = Interface::new(path.as_ref()).map_err(|error| io_errno(&error))?;
        let syspath = iface.syspath().to_path_buf();
        let devtype = iface.attr("devtype").ok();
        let profile = profile_for_device(config, &syspath, devtype.as_deref());
        let mut iface = iface;
        iface.watch(true).map_err(|error| io_errno(&error))?;
        let requested = requested_interfaces(profile, config);
        let mut pending = requested;
        let opened_result = iface.open(requested);
        let opened = match &opened_result {
            Ok(mask) => *mask,
            Err(error) => error.opened(),
        };
        if opened.is_empty()
            && let Err(error) = opened_result
        {
            return Err(io_errno(error.source()));
        }
        pending &= !opened;
        let (gamepad, desktop) = create_outputs(profile, config, &backend, outputs_enabled)?;
        Ok(Self {
            syspath,
            profile,
            iface,
            gamepad,
            desktop,
            engine: DeviceEngine::new(validated, profile),
            actions: Vec::with_capacity(32),
            opened_ifaces: opened,
            pending_ifaces: pending,
            trace: None,
            config: config.clone(),
            backend,
            outputs_enabled,
            capture: false,
        })
    }
    pub fn set_trace_sink<F: FnMut(&str) + 'static>(&mut self, filter: TraceFilter, sink: F) {
        self.set_trace_sink_with_sequence(filter, Rc::new(Cell::new(0)), sink);
    }
    pub(crate) fn set_trace_sink_with_sequence<F: FnMut(&str) + 'static>(
        &mut self,
        filter: TraceFilter,
        sequence: Rc<Cell<u64>>,
        sink: F,
    ) {
        self.trace = Some(TraceContext::new(filter, sequence, sink));
    }
    pub fn path(&self) -> &Path {
        &self.syspath
    }
    fn requested_interfaces(&self) -> InterfaceMask {
        if self.capture {
            InterfaceMask::ALL
        } else {
            requested_interfaces(self.profile, &self.config)
        }
    }
    pub(crate) fn set_capture(&mut self, enabled: bool) -> Result<(), i32> {
        if enabled == self.capture {
            return Ok(());
        }
        self.capture = enabled;
        let desired = self.requested_interfaces();
        self.iface.close(self.iface.opened() & !desired);
        self.opened_ifaces = self.iface.opened();
        self.pending_ifaces = desired & !self.opened_ifaces;
        self.retry_open()
    }
    pub fn retry_open(&mut self) -> Result<(), i32> {
        if self.pending_ifaces.is_empty() {
            return Ok(());
        }
        let result = self.iface.open(self.pending_ifaces);
        let opened = match &result {
            Ok(mask) => *mask,
            Err(error) => error.opened(),
        };
        self.opened_ifaces = opened;
        self.pending_ifaces &= !self.opened_ifaces;
        if !(self.opened_ifaces & requested_interfaces(self.profile, &self.config)).is_empty() {
            if result.is_err() && !self.pending_ifaces.is_empty() {
                return Ok(());
            }
            return Ok(());
        }
        match result {
            Ok(_) => Err(-libc::ENODEV),
            Err(error) => Err(io_errno(error.source())),
        }
    }
    pub fn handle_watch(&mut self) -> Result<(), i32> {
        let opened = self.iface.opened();
        let lost = self.opened_ifaces & !opened & requested_interfaces(self.profile, &self.config);
        if !lost.is_empty() {
            self.gamepad.take();
            self.desktop.take();
            self.engine.process(EngineInput::Reset, &mut self.actions);
            self.recreate_outputs()?;
        }
        self.opened_ifaces = opened;
        self.pending_ifaces = self.requested_interfaces() & !opened;
        self.retry_open()
    }
    fn recreate_outputs(&mut self) -> Result<(), i32> {
        let (gamepad, desktop) = create_outputs(
            self.profile,
            &self.config,
            &self.backend,
            self.outputs_enabled,
        )?;
        self.gamepad = gamepad;
        self.desktop = desktop;
        Ok(())
    }
    pub fn drain(&mut self) -> Result<BridgeAction, i32> {
        self.drain_with(|_, _| {})
    }
    pub fn drain_with<F>(&mut self, mut observer: F) -> Result<BridgeAction, i32>
    where
        F: FnMut(&Path, &Event),
    {
        for _ in 0..MAX_EVENTS_PER_DRAIN {
            match self.iface.dispatch() {
                Ok(event) => {
                    self.trace(&event);
                    observer(&self.syspath, &event);
                    match self.handle_event(&event)? {
                        BridgeAction::Continue => {}
                        BridgeAction::Gone => return Ok(BridgeAction::Gone),
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(BridgeAction::Continue);
                }
                Err(e) => return Err(io_errno(&e)),
            }
        }
        Ok(BridgeAction::Continue)
    }
    fn trace(&mut self, event: &Event) {
        if let Some(trace) = self.trace.as_mut() {
            trace.emit(&self.syspath, event);
        }
    }
    pub fn handle_event(&mut self, event: &Event) -> Result<BridgeAction, i32> {
        match event.kind {
            EventKind::Gone => return Ok(BridgeAction::Gone),
            EventKind::Watch => self.handle_watch()?,
            kind => {
                if forwards_to_engine(kind, self.profile, &self.config)
                    && let Some(input) = engine_input(kind)
                {
                    self.process(input)?;
                }
            }
        }
        Ok(BridgeAction::Continue)
    }
    pub fn pointer_active(&self) -> bool {
        self.engine.pointer_active()
    }
    pub fn tick_pointer(&mut self) -> Result<(), i32> {
        self.process(EngineInput::PointerTick)
    }
    fn process(&mut self, input: EngineInput) -> Result<(), i32> {
        self.actions.clear();
        self.engine.process(input, &mut self.actions);
        if !self.outputs_enabled {
            return Ok(());
        }
        for action in &self.actions {
            let target = match *action {
                OutputAction::Key(target, ..)
                | OutputAction::Abs(target, ..)
                | OutputAction::Rel(target, ..)
                | OutputAction::Sync(target) => target,
            };
            let device = match target {
                OutputDevice::Gamepad => &mut self.gamepad,
                OutputDevice::Desktop => &mut self.desktop,
            };
            if let Some(device) = device {
                match *action {
                    OutputAction::Key(_, code, value) => device.emit_key(code, value)?,
                    OutputAction::Abs(_, code, value) => device.emit_abs(code, value)?,
                    OutputAction::Rel(_, code, value) => device.emit_rel(code, value)?,
                    OutputAction::Sync(_) => device.syn()?,
                }
            }
        }
        Ok(())
    }
}

fn forwards_to_engine(kind: EventKind, profile: Profile, config: &Config) -> bool {
    kind.interface()
        .is_some_and(|interface| requested_interfaces(profile, config).contains(interface))
}

fn engine_input(kind: EventKind) -> Option<EngineInput> {
    if let Some((code, state)) = key_event(kind) {
        return Some(EngineInput::Key { code, state });
    }
    if let EventKind::Ir(values) = kind {
        return Some(EngineInput::Ir(IrFrame {
            points: values.map(|value| IrPoint {
                valid: valid_ir_point(&value),
                x: value.x,
                y: value.y,
            }),
        }));
    }
    let motion = match kind {
        EventKind::Accel(_) => MotionKind::Accel,
        EventKind::MotionPlus(_) => MotionKind::MotionPlus,
        EventKind::NunchukMove(_) => MotionKind::Nunchuk,
        EventKind::ClassicControllerMove(_) => MotionKind::Classic,
        EventKind::ProControllerMove(_) => MotionKind::Pro,
        EventKind::GuitarMove(_) => MotionKind::Guitar,
        EventKind::DrumsMove(_) => MotionKind::Drums,
        EventKind::BalanceBoard(_) => MotionKind::Balance,
        _ => return None,
    };
    let mut axes = [Abs3 { x: 0, y: 0, z: 0 }; 8];
    for (target, value) in axes.iter_mut().zip(axis_events(&kind)) {
        *target = Abs3 {
            x: value.x,
            y: value.y,
            z: value.z,
        };
    }
    Some(EngineInput::Motion { kind: motion, axes })
}

impl<B: Backend + Clone> Drop for BridgeDevice<B> {
    fn drop(&mut self) {
        self.gamepad.take();
        self.desktop.take();
    }
}
fn profile_for_device(config: &Config, syspath: &Path, devtype: Option<&[u8]>) -> Profile {
    let syspath = syspath.to_string_lossy();
    let devtype = devtype
        .and_then(|value| std::str::from_utf8(value).ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    config.profile_for_device(Some(&syspath), devtype)
}
type OutputPair<B> = (Option<VirtualDevice<B>>, Option<VirtualDevice<B>>);
fn create_outputs<B: Backend + Clone>(
    profile: Profile,
    config: &Config,
    backend: &B,
    outputs_enabled: bool,
) -> Result<OutputPair<B>, i32> {
    if !outputs_enabled {
        return Ok((None, None));
    }
    let gamepad = if needs_gamepad(profile, config) {
        Some(VirtualDevice::with_backend(
            crate::uinput::UINPUT_PATH,
            VirtualKind::Controller,
            backend.clone(),
        )?)
    } else {
        None
    };
    let desktop = if needs_desktop(profile, config) {
        Some(VirtualDevice::with_backend(
            crate::uinput::UINPUT_PATH,
            VirtualKind::Desktop,
            backend.clone(),
        )?)
    } else {
        None
    };
    Ok((gamepad, desktop))
}
pub fn requested_interfaces(p: Profile, c: &Config) -> InterfaceMask {
    let mut interfaces = InterfaceMask::empty();
    if p.contains(Profile::GAMEPAD) {
        interfaces = InterfaceMask::ALL & !InterfaceMask::IR;
    }
    if p.contains(Profile::DESKTOP) {
        interfaces |= InterfaceMask::CORE | InterfaceMask::IR;
    }
    if c.aim_mode != wiiland_core::AimMode::Off {
        interfaces |= match c.aim_source {
            wiiland_core::AimSource::Ir => InterfaceMask::IR,
            wiiland_core::AimSource::MotionPlus => InterfaceMask::MOTION_PLUS,
            wiiland_core::AimSource::Accelerometer => InterfaceMask::ACCEL,
            wiiland_core::AimSource::Auto => {
                InterfaceMask::IR | InterfaceMask::MOTION_PLUS | InterfaceMask::ACCEL
            }
        };
        if matches!(
            c.aim_activation,
            wiiland_core::AimActivation::Z | wiiland_core::AimActivation::C
        ) {
            interfaces |= InterfaceMask::NUNCHUK;
        }
    }
    interfaces
}
fn valid_ir_point(abs: &Axis3) -> bool {
    abs.x != 1023 || abs.y != 1023
}
fn key_event(kind: EventKind) -> Option<(u32, u32)> {
    let key = match kind {
        EventKind::Key(key)
        | EventKind::NunchukKey(key)
        | EventKind::ClassicControllerKey(key)
        | EventKind::ProControllerKey(key)
        | EventKind::GuitarKey(key)
        | EventKind::DrumsKey(key) => key,
        _ => return None,
    };
    Some((button_code(key.button)?, button_state(key.state)?))
}

fn axis_events(kind: &EventKind) -> &[Axis3] {
    match kind {
        EventKind::Accel(value) | EventKind::MotionPlus(value) => std::slice::from_ref(value),
        EventKind::Ir(values) | EventKind::BalanceBoard(values) => values,
        EventKind::NunchukMove(values) | EventKind::ProControllerMove(values) => values,
        EventKind::ClassicControllerMove(values) | EventKind::GuitarMove(values) => values,
        EventKind::DrumsMove(values) => values,
        _ => &[],
    }
}

fn monotonic_time_us() -> i64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
        return 0;
    }
    now.tv_sec
        .saturating_mul(1_000_000)
        .saturating_add(now.tv_nsec / 1_000)
}

fn format_trace_line(sequence: u64, monotonic_us: i64, syspath: &Path, event: &Event) -> String {
    let payload = if let Some((code, state)) = key_event(event.kind) {
        TracePayload::Key(KeyPayload { code, state })
    } else {
        let axes = axis_events(&event.kind);
        if axes.is_empty() {
            TracePayload::None
        } else {
            TracePayload::Axes(
                axes.iter()
                    .map(|value| AbsPayload {
                        x: value.x,
                        y: value.y,
                        z: value.z,
                    })
                    .collect(),
            )
        }
    };

    let mut line = TraceEvent::new(
        sequence,
        Some(monotonic_us),
        syspath.to_string_lossy(),
        event_type_code(event.kind),
        payload,
    )
    .format_line();
    if line.ends_with('\n') {
        line.pop();
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uinput::{RecordingBackend, RecordingOp};
    use std::cell::RefCell;
    use wiiland_core::{AimActivation, AimMode, AimSource, DeviceRule, DeviceRuleKind};
    use wiiland_hid::ButtonEvent;

    #[test]
    fn diagnostic_only_interfaces_never_change_virtual_input() {
        let config = Config::default();
        let key = EventKind::NunchukKey(ButtonEvent {
            button: Button::C,
            state: ButtonState::Pressed,
        });
        assert!(!forwards_to_engine(key, Profile::DESKTOP, &config));
        assert!(forwards_to_engine(key, Profile::GAMEPAD, &config));
        assert!(!forwards_to_engine(
            EventKind::Accel(Axis3::default()),
            Profile::DESKTOP,
            &config
        ));
        assert!(!forwards_to_engine(
            EventKind::Ir([Axis3::default(); 4]),
            Profile::GAMEPAD,
            &config
        ));
    }

    #[test]
    fn button_states_preserve_linux_input_values() {
        assert_eq!(button_state(ButtonState::Released), Some(0));
        assert_eq!(button_state(ButtonState::Pressed), Some(1));
        assert_eq!(button_state(ButtonState::Repeated), Some(2));
    }

    #[test]
    fn disabled_outputs_remain_absent_when_recreated_after_watch() {
        let backend = RecordingBackend::new();
        let config = Config {
            profile: Profile::BOTH,
            ..Config::default()
        };

        let initial = create_outputs(config.profile, &config, &backend, false).unwrap();
        assert!(initial.0.is_none());
        assert!(initial.1.is_none());
        let refreshed = create_outputs(config.profile, &config, &backend, false).unwrap();
        assert!(refreshed.0.is_none());
        assert!(refreshed.1.is_none());
        assert!(backend.operations().is_empty());
    }

    #[test]
    fn enabled_outputs_use_the_recording_backend() {
        let backend = RecordingBackend::new();
        let config = Config {
            profile: Profile::BOTH,
            ..Config::default()
        };

        let outputs = create_outputs(config.profile, &config, &backend, true).unwrap();
        assert!(outputs.0.is_some());
        assert!(outputs.1.is_some());
        assert_eq!(
            backend
                .operations()
                .iter()
                .filter(|op| matches!(op, RecordingOp::Open(_)))
                .count(),
            2
        );
    }

    #[test]
    fn profile_selection_uses_syspath_and_trimmed_devtype() {
        let config = Config {
            profile: Profile::GAMEPAD,
            device_rules: vec![
                DeviceRule {
                    kind: DeviceRuleKind::Syspath,
                    match_text: "wii-red".into(),
                    profile: Profile::DESKTOP,
                },
                DeviceRule {
                    kind: DeviceRuleKind::Devtype,
                    match_text: "balanceboard".into(),
                    profile: Profile::BOTH,
                },
            ],
            ..Config::default()
        };

        assert_eq!(
            profile_for_device(
                &config,
                Path::new("/sys/devices/wii-red"),
                Some(b"balanceboard\n")
            ),
            Profile::BOTH
        );
    }

    #[test]
    fn requested_interfaces_include_nunchuk_for_z_and_c_activation() {
        for activation in [AimActivation::Z, AimActivation::C] {
            let config = Config {
                profile: Profile::DESKTOP,
                aim_mode: AimMode::Mouse,
                aim_source: AimSource::Accelerometer,
                aim_activation: activation,
                ..Config::default()
            };
            assert!(requested_interfaces(config.profile, &config).contains(InterfaceMask::NUNCHUK));
        }

        let config = Config {
            profile: Profile::DESKTOP,
            aim_mode: AimMode::Mouse,
            aim_source: AimSource::Accelerometer,
            aim_activation: AimActivation::B,
            ..Config::default()
        };

        assert!(!requested_interfaces(config.profile, &config).contains(InterfaceMask::NUNCHUK));
    }

    #[test]
    fn gamepad_forwarding_predicate_includes_right_stick_aim_output() {
        let config = Config {
            profile: Profile::DESKTOP,
            aim_mode: AimMode::RightStick,
            ..Config::default()
        };
        assert!(needs_gamepad(config.profile, &config));
        assert!(!config.profile.contains(Profile::GAMEPAD));
    }

    #[test]
    fn desktop_pointer_controls_require_the_desktop_profile_not_only_an_aim_output() {
        let config = Config {
            profile: Profile::GAMEPAD,
            aim_mode: AimMode::Mouse,
            ..Config::default()
        };
        assert!(needs_desktop(config.profile, &config));
        assert!(!config.profile.contains(Profile::DESKTOP));
    }

    #[test]
    fn trace_lines_use_emission_time_and_include_name_type_and_key_payload() {
        let event = Event {
            time: wiiland_hid::Timestamp {
                seconds: 99,
                microseconds: 999,
            },
            kind: EventKind::NunchukKey(ButtonEvent {
                button: Button::Z,
                state: ButtonState::Pressed,
            }),
        };

        assert_eq!(
            format_trace_line(7, 12_000_034, Path::new("/sys/wii0"), &event),
            "time=12.000034 seq=7 /sys/wii0 nunchuk-key type=10 key=20 state=1"
        );
    }

    #[test]
    fn trace_lines_include_all_eight_absolute_payloads() {
        let event = Event {
            time: wiiland_hid::Timestamp {
                seconds: 0,
                microseconds: 0,
            },
            kind: EventKind::DrumsMove(std::array::from_fn(|i| Axis3 {
                x: i as i32,
                y: -(i as i32),
                z: (i * 10) as i32,
            })),
        };

        assert_eq!(
            format_trace_line(8, 1_000_002, Path::new("/sys/wii1"), &event),
            concat!(
                "time=1.000002 seq=8 /sys/wii1 drums-move type=13",
                " abs0=0,0,0 abs1=1,-1,10 abs2=2,-2,20 abs3=3,-3,30",
                " abs4=4,-4,40 abs5=5,-5,50 abs6=6,-6,60 abs7=7,-7,70"
            )
        );
    }

    #[test]
    fn trace_contexts_share_sequence_and_timestamp_lifecycle_events_at_emission() {
        let sequence = Rc::new(Cell::new(0));
        let now = Rc::new(Cell::new(0));
        let lines = Rc::new(RefCell::new(Vec::new()));

        let mut first = TraceContext::with_clock(
            TraceFilter::All,
            Rc::clone(&sequence),
            {
                let now = Rc::clone(&now);
                move || now.get()
            },
            {
                let lines = Rc::clone(&lines);
                move |line| lines.borrow_mut().push(line.to_owned())
            },
        );
        let mut second = TraceContext::with_clock(
            TraceFilter::All,
            Rc::clone(&sequence),
            {
                let now = Rc::clone(&now);
                move || now.get()
            },
            {
                let lines = Rc::clone(&lines);
                move |line| lines.borrow_mut().push(line.to_owned())
            },
        );
        let watch = Event {
            time: wiiland_hid::Timestamp {
                seconds: 0,
                microseconds: 0,
            },
            kind: EventKind::Watch,
        };
        let gone = Event {
            time: wiiland_hid::Timestamp {
                seconds: 0,
                microseconds: 0,
            },
            kind: EventKind::Gone,
        };

        now.set(41_000_007);
        first.emit(Path::new("/sys/wii0"), &watch);
        now.set(42_000_008);
        second.emit(Path::new("/sys/wii1"), &gone);

        assert_eq!(sequence.get(), 2);
        assert_eq!(
            &*lines.borrow(),
            &[
                "time=41.000007 seq=1 /sys/wii0 watch type=7",
                "time=42.000008 seq=2 /sys/wii1 gone type=16",
            ]
        );
    }
}
