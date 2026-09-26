//! Windows bridge output adapters.
//!
//! Gamepad state is published through the lease-based WiiLand output broker.
//! Desktop keyboard and relative mouse actions use the current process's user
//! input stream and are rejected for elevated, high-integrity, UIAccess, and
//! session-0 processes. Neither sink silently reconnects after an output error.

use super::{
    BridgeAction, TraceContext, engine_input, forwards_to_engine, io_errno, profile_for_device,
    requested_interfaces,
};
use crate::windows_output_worker::{OutputWorker, OutputWorkerStatus};
use std::cell::Cell;
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::path::{Path, PathBuf};
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use wiiland_core::engine::{
    DeviceEngine, EngineInput, OutputAction, OutputDevice, needs_desktop, needs_gamepad,
};
use wiiland_core::mapping::{CORE_AXES, CORE_KEYS, axis_info};
use wiiland_core::{Config, Profile, TraceFilter};
use wiiland_hid::{Event, EventKind, Interface, InterfaceMask};
use wiiland_ipc::DeviceInfo;
use wiiland_output_service::{OutputClient, OutputLease, OutputReport, ReportId};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, IsValidSid, TOKEN_ELEVATION,
    TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenElevation,
    TokenIntegrityLevel, TokenSessionId, TokenUIAccess,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEINPUT, SendInput,
};

const GAMEPAD_REPORT_SIZE: usize = 17;
const AXES_REPORT_SIZE: usize = 88;
const _: [(); 28] = [(); CORE_KEYS.len()];
const _: [(); 22] = [(); CORE_AXES.len()];
const _: [(); AXES_REPORT_SIZE] = [(); CORE_AXES.len() * size_of::<i32>()];

const DESKTOP_KEY_USAGES: [(u16, u8); 5] = [
    (28, 0x28),  // Enter
    (1, 0x29),   // Escape
    (125, 0xe3), // Left GUI
    (104, 0x4b), // Page Up
    (109, 0x4e), // Page Down
];
const DESKTOP_MOUSE_LEFT: u16 = 0x110;
const DESKTOP_MOUSE_RIGHT: u16 = 0x111;
const SECURITY_MANDATORY_HIGH_RID_VALUE: u32 = 0x3000;

/// A single broker lease for the fixed gamepad and supplemental-axis reports.
/// The broker is not used for desktop reports.
pub struct WindowsOutputSession {
    broker: Option<BrokerGamepad>,
    desktop: Option<DesktopInput>,
    failed: Option<i32>,
    closed: bool,
}

impl WindowsOutputSession {
    /// Start only the requested sinks. A desktop-only session never connects
    /// to the privileged output broker or reserves a VHID lease.
    pub fn connect(gamepad: bool, desktop: bool) -> io::Result<Self> {
        Self::connect_inner(gamepad, desktop, None)
    }

    pub(crate) fn connect_cancellable(
        gamepad: bool,
        desktop: bool,
        stop: &AtomicBool,
    ) -> io::Result<Self> {
        Self::connect_inner(gamepad, desktop, Some(stop))
    }

    fn connect_inner(gamepad: bool, desktop: bool, stop: Option<&AtomicBool>) -> io::Result<Self> {
        check_stopping(stop)?;
        let desktop = desktop.then(DesktopInput::new).transpose()?;
        let broker = if gamepad {
            Some(match stop {
                Some(stop) => BrokerGamepad::connect_cancellable(stop)?,
                None => BrokerGamepad::connect()?,
            })
        } else {
            None
        };
        Ok(Self {
            broker,
            desktop,
            failed: None,
            closed: false,
        })
    }

    pub fn gamepad_active(&self) -> bool {
        !self.closed && self.failed.is_none() && self.broker.is_some()
    }

    pub fn desktop_active(&self) -> bool {
        !self.closed && self.failed.is_none() && self.desktop.is_some()
    }

    /// Process an ordered synchronous batch. Key actions publish immediately;
    /// absolute and relative changes remain staged until their matching Sync.
    pub fn process_actions(&mut self, actions: &[OutputAction]) -> Result<(), i32> {
        self.process_actions_inner(actions, None)
    }

    pub(crate) fn process_action_cancellable(
        &mut self,
        action: OutputAction,
        stop: &AtomicBool,
    ) -> Result<(), i32> {
        self.process_actions_inner(std::slice::from_ref(&action), Some(stop))
    }

    fn process_actions_inner(
        &mut self,
        actions: &[OutputAction],
        stop: Option<&AtomicBool>,
    ) -> Result<(), i32> {
        self.require_live()?;
        for &action in actions {
            check_stopping(stop).map_err(|_| -libc::ECANCELED)?;
            let result = match action {
                OutputAction::Key(OutputDevice::Gamepad, code, value) => {
                    self.broker.as_mut().map_or_else(
                        || Err(no_output_sink()),
                        |broker| match stop {
                            Some(stop) => broker.key_with_stop(code, value, Some(stop)),
                            None => broker.key(code, value),
                        },
                    )
                }
                OutputAction::Abs(OutputDevice::Gamepad, code, value) => self
                    .broker
                    .as_mut()
                    .map_or_else(|| Err(no_output_sink()), |broker| broker.axis(code, value)),
                OutputAction::Rel(OutputDevice::Gamepad, _, _) => {
                    if self.broker.is_some() {
                        Err(invalid_input("gamepad relative actions are unsupported"))
                    } else {
                        Err(no_output_sink())
                    }
                }
                OutputAction::Sync(OutputDevice::Gamepad) => self.broker.as_mut().map_or_else(
                    || Err(no_output_sink()),
                    |broker| match stop {
                        Some(stop) => broker.sync_with_stop(Some(stop)),
                        None => broker.sync(),
                    },
                ),
                OutputAction::Key(OutputDevice::Desktop, code, value) => self
                    .desktop
                    .as_mut()
                    .map_or_else(|| Err(no_output_sink()), |desktop| desktop.key(code, value)),
                OutputAction::Abs(OutputDevice::Desktop, _, _) => {
                    if self.desktop.is_some() {
                        Err(invalid_input(
                            "desktop absolute-axis actions are unsupported",
                        ))
                    } else {
                        Err(no_output_sink())
                    }
                }
                OutputAction::Rel(OutputDevice::Desktop, code, value) => {
                    self.desktop.as_mut().map_or_else(
                        || Err(no_output_sink()),
                        |desktop| desktop.relative(code, value),
                    )
                }
                OutputAction::Sync(OutputDevice::Desktop) => self
                    .desktop
                    .as_mut()
                    .map_or_else(|| Err(no_output_sink()), DesktopInput::sync),
            };
            if let Err(error) = result {
                if is_stop_cancellation(&error, stop) {
                    return Err(-libc::ECANCELED);
                }
                return self.fail(output_errno(&error));
            }
        }
        Ok(())
    }

    /// Republish the committed gamepad state. Pending unsynchronized axes are
    /// excluded from refresh reports.
    pub fn refresh(&mut self) -> Result<(), i32> {
        self.refresh_inner(None)
    }

    pub(crate) fn refresh_cancellable(&mut self, stop: &AtomicBool) -> Result<(), i32> {
        self.refresh_inner(Some(stop))
    }

    fn refresh_inner(&mut self, stop: Option<&AtomicBool>) -> Result<(), i32> {
        self.require_live()?;
        check_stopping(stop).map_err(|_| -libc::ECANCELED)?;
        let result = self.broker.as_mut().map_or(Ok(()), |broker| match stop {
            Some(stop) => broker.publish_state_with_stop(Some(stop)),
            None => broker.publish_state(),
        });
        match result {
            Ok(()) => Ok(()),
            Err(error) if is_stop_cancellation(&error, stop) => Err(-libc::ECANCELED),
            Err(error) => self.fail(output_errno(&error)),
        }
    }

    /// Release all held desktop inputs and replace the gamepad lease. A broken
    /// broker connection is terminal; reset never reconnects or replays state.
    pub fn reset(&mut self) -> Result<(), i32> {
        self.reset_inner(None)
    }

    pub(crate) fn reset_cancellable(&mut self, stop: &AtomicBool) -> Result<(), i32> {
        self.reset_inner(Some(stop))
    }

    fn reset_inner(&mut self, stop: Option<&AtomicBool>) -> Result<(), i32> {
        self.require_live()?;
        check_stopping(stop).map_err(|_| -libc::ECANCELED)?;
        let desktop_result = self
            .desktop
            .as_mut()
            .map_or(Ok(()), DesktopInput::release_all);
        if let Err(error) = desktop_result {
            return self.fail(output_errno(&error));
        }
        check_stopping(stop).map_err(|_| -libc::ECANCELED)?;
        let broker_result = self.broker.as_mut().map_or(Ok(()), |broker| match stop {
            Some(stop) => broker.reset_with_stop(Some(stop)),
            None => broker.reset(),
        });
        match broker_result {
            Ok(()) => Ok(()),
            Err(error) if is_stop_cancellation(&error, stop) => Err(-libc::ECANCELED),
            Err(error) => self.fail(output_errno(&error)),
        }
    }

    /// Neutralize and destroy the lease, and release any user-input keys or
    /// buttons. The broker's destroy operation neutralizes every report before
    /// revoking the lease.
    pub fn close(&mut self) -> Result<(), i32> {
        if self.closed {
            return self.failed.map_or(Ok(()), Err);
        }
        let mut first_error = None;
        if let Some(mut desktop) = self.desktop.take()
            && let Err(error) = desktop.release_all()
        {
            first_error = Some(output_errno(&error));
        }
        if let Some(mut broker) = self.broker.take()
            && let Err(error) = broker.close()
            && first_error.is_none()
        {
            first_error = Some(output_errno(&error));
        }
        self.closed = true;
        if let Some(error) = first_error {
            self.failed = Some(error);
            Err(error)
        } else {
            Ok(())
        }
    }

    fn require_live(&self) -> Result<(), i32> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.closed {
            return Err(-libc::EIO);
        }
        Ok(())
    }

    fn fail(&mut self, error: i32) -> Result<(), i32> {
        self.abort();
        self.failed = Some(error);
        Err(error)
    }

    fn abort(&mut self) {
        if let Some(mut desktop) = self.desktop.take() {
            let _ = desktop.release_all();
        }
        if let Some(mut broker) = self.broker.take() {
            let _ = broker.close();
        }
        self.closed = true;
    }
}

impl Drop for WindowsOutputSession {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.close();
        }
    }
}

struct BrokerGamepad {
    client: OutputClient,
    lease: Option<OutputLease>,
    state: GamepadState,
}

impl BrokerGamepad {
    fn connect() -> io::Result<Self> {
        Self::connect_with_stop(None)
    }

    fn connect_cancellable(stop: &AtomicBool) -> io::Result<Self> {
        Self::connect_with_stop(Some(stop))
    }

    fn connect_with_stop(stop: Option<&AtomicBool>) -> io::Result<Self> {
        check_stopping(stop)?;
        let mut client = OutputClient::connect()?;
        check_stopping(stop)?;
        let lease = client.create()?;
        if let Err(error) = check_stopping(stop) {
            let _ = client.destroy(lease);
            return Err(error);
        }
        let mut output = Self {
            client,
            lease: Some(lease),
            state: GamepadState::default(),
        };
        let publish_result = match stop {
            Some(stop) => output.publish_state_with_stop(Some(stop)),
            None => output.publish_state(),
        };
        if let Err(error) = publish_result {
            let _ = output.close();
            return Err(error);
        }
        Ok(output)
    }

    fn key(&mut self, code: u16, value: u32) -> io::Result<()> {
        self.key_with_stop(code, value, None)
    }

    fn key_with_stop(
        &mut self,
        code: u16,
        value: u32,
        stop: Option<&AtomicBool>,
    ) -> io::Result<()> {
        check_stopping(stop)?;
        if value > 2 {
            return Err(invalid_input("gamepad key value must be 0, 1, or 2"));
        }
        let Some(index) = CORE_KEYS.iter().position(|&key| key == code) else {
            return Err(invalid_input("unknown core gamepad button code"));
        };
        if value == 0 {
            self.state.buttons &= !(1 << index);
        } else {
            self.state.buttons |= 1 << index;
        }
        let report = encode_gamepad_report(self.state.buttons, &self.state.committed_axes);
        self.submit_with_stop(ReportId::Gamepad, &report, stop)
    }

    fn axis(&mut self, code: u16, value: i32) -> io::Result<()> {
        let Some(index) = CORE_AXES.iter().position(|&axis| axis == code) else {
            return Err(invalid_input("unknown core gamepad axis code"));
        };
        let Some(info) = axis_info(code) else {
            return Err(invalid_input("core gamepad axis metadata is missing"));
        };
        if !(info.minimum..=info.maximum).contains(&value) {
            return Err(invalid_input("gamepad axis is outside its HID range"));
        }
        self.state.pending_axes[index] = value;
        self.state.axes_dirty = true;
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.sync_with_stop(None)
    }

    fn sync_with_stop(&mut self, stop: Option<&AtomicBool>) -> io::Result<()> {
        if !self.state.axes_dirty {
            return Ok(());
        }
        let gamepad = encode_gamepad_report(self.state.buttons, &self.state.pending_axes);
        let supplemental = encode_supplemental_axes(&self.state.pending_axes);
        self.submit_with_stop(ReportId::Gamepad, &gamepad, stop)?;
        self.submit_with_stop(ReportId::SupplementalAxes, &supplemental, stop)?;
        self.state.committed_axes = self.state.pending_axes;
        self.state.axes_dirty = false;
        Ok(())
    }

    fn reset(&mut self) -> io::Result<()> {
        self.reset_with_stop(None)
    }

    fn reset_with_stop(&mut self, stop: Option<&AtomicBool>) -> io::Result<()> {
        check_stopping(stop)?;
        self.close()?;
        check_stopping(stop)?;
        let lease = self.client.create()?;
        self.lease = Some(lease);
        if let Err(error) = check_stopping(stop) {
            let _ = self.close();
            return Err(error);
        }
        self.state = GamepadState::default();
        if let Err(error) = self.publish_state_with_stop(stop) {
            let _ = self.close();
            return Err(error);
        }
        Ok(())
    }

    fn publish_state(&mut self) -> io::Result<()> {
        self.publish_state_with_stop(None)
    }

    fn publish_state_with_stop(&mut self, stop: Option<&AtomicBool>) -> io::Result<()> {
        let gamepad = encode_gamepad_report(self.state.buttons, &self.state.committed_axes);
        let supplemental = encode_supplemental_axes(&self.state.committed_axes);
        self.submit_with_stop(ReportId::Gamepad, &gamepad, stop)?;
        self.submit_with_stop(ReportId::SupplementalAxes, &supplemental, stop)
    }

    fn submit_with_stop(
        &mut self,
        id: ReportId,
        bytes: &[u8],
        stop: Option<&AtomicBool>,
    ) -> io::Result<()> {
        check_stopping(stop)?;
        let report = OutputReport::new(id, bytes)?;
        let lease = self.lease.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "gamepad lease is not active")
        })?;
        check_stopping(stop)?;
        self.client.submit(lease, &report)
    }

    fn close(&mut self) -> io::Result<()> {
        let Some(lease) = self.lease.take() else {
            return Ok(());
        };
        self.client.destroy(lease)
    }
}

#[derive(Clone, Copy)]
struct GamepadState {
    buttons: u32,
    committed_axes: [i32; CORE_AXES.len()],
    pending_axes: [i32; CORE_AXES.len()],
    axes_dirty: bool,
}

impl Default for GamepadState {
    fn default() -> Self {
        Self {
            buttons: 0,
            committed_axes: [0; CORE_AXES.len()],
            pending_axes: [0; CORE_AXES.len()],
            axes_dirty: false,
        }
    }
}

fn encode_gamepad_report(buttons: u32, axes: &[i32; CORE_AXES.len()]) -> [u8; GAMEPAD_REPORT_SIZE] {
    let mut report = [0; GAMEPAD_REPORT_SIZE];
    report[..4].copy_from_slice(&buttons.to_le_bytes());
    report[4] = 8;
    // The fixed gamepad report selects CORE_AXES core codes 0, 1, 3, 4, 2, 5:
    // four signed stick values followed by the two unsigned trigger values.
    for (offset, axis_index) in [0usize, 1, 2, 3].into_iter().enumerate() {
        let bytes = axes[axis_index].to_le_bytes();
        let start = 5 + offset * 2;
        report[start..start + 2].copy_from_slice(&bytes[..2]);
    }
    for (offset, axis_index) in [4usize, 5].into_iter().enumerate() {
        let bytes = axes[axis_index].to_le_bytes();
        let start = 13 + offset * 2;
        report[start..start + 2].copy_from_slice(&bytes[..2]);
    }
    report
}

fn encode_supplemental_axes(axes: &[i32; CORE_AXES.len()]) -> [u8; AXES_REPORT_SIZE] {
    let mut report = [0; AXES_REPORT_SIZE];
    for (index, value) in axes.iter().enumerate() {
        let start = index * size_of::<i32>();
        report[start..start + size_of::<i32>()].copy_from_slice(&value.to_le_bytes());
    }
    report
}

struct DesktopInput {
    keys: [bool; DESKTOP_KEY_USAGES.len()],
    mouse_buttons: u8,
    pending_relative: [i64; 2],
    relative_dirty: bool,
}

impl DesktopInput {
    fn new() -> io::Result<Self> {
        ensure_safe_desktop_token()?;
        Ok(Self {
            keys: [false; DESKTOP_KEY_USAGES.len()],
            mouse_buttons: 0,
            pending_relative: [0; 2],
            relative_dirty: false,
        })
    }

    fn key(&mut self, code: u16, value: u32) -> io::Result<()> {
        if value > 2 {
            return Err(invalid_input("desktop key value must be 0, 1, or 2"));
        }
        if let Some(index) = DESKTOP_KEY_USAGES
            .iter()
            .position(|&(key_code, _)| key_code == code)
        {
            let usage = DESKTOP_KEY_USAGES[index].1;
            let (virtual_key, extended) = virtual_key_for_usage(usage)
                .ok_or_else(|| invalid_input("desktop keyboard usage is unmapped"))?;
            let down = value != 0;
            let mut flags = if extended { KEYEVENTF_EXTENDEDKEY } else { 0 };
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            if down {
                self.keys[index] = true;
            }
            send_keyboard(virtual_key, flags)?;
            self.keys[index] = down;
            return Ok(());
        }
        if code == DESKTOP_MOUSE_LEFT || code == DESKTOP_MOUSE_RIGHT {
            let (bit, down_flag, up_flag) = if code == DESKTOP_MOUSE_LEFT {
                (1, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP)
            } else {
                (2, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP)
            };
            let down = value != 0;
            if down {
                self.mouse_buttons |= bit;
            }
            send_mouse_button(if down { down_flag } else { up_flag })?;
            if down {
                self.mouse_buttons |= bit;
            } else {
                self.mouse_buttons &= !bit;
            }
            return Ok(());
        }
        Err(invalid_input("unknown desktop key or mouse button code"))
    }

    fn relative(&mut self, code: u16, value: i32) -> io::Result<()> {
        let index = match code {
            0 => 0,
            1 => 1,
            _ => return Err(invalid_input("unknown desktop relative axis code")),
        };
        self.pending_relative[index] =
            self.pending_relative[index]
                .checked_add(i64::from(value))
                .ok_or_else(|| invalid_input("desktop relative movement overflow"))?;
        self.relative_dirty = true;
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        if !self.relative_dirty {
            return Ok(());
        }
        let dx = i32::try_from(self.pending_relative[0])
            .map_err(|_| invalid_input("desktop relative X exceeds the SendInput range"))?;
        let dy = i32::try_from(self.pending_relative[1])
            .map_err(|_| invalid_input("desktop relative Y exceeds the SendInput range"))?;
        send_relative_mouse(dx, dy)?;
        self.pending_relative = [0; 2];
        self.relative_dirty = false;
        Ok(())
    }

    fn release_all(&mut self) -> io::Result<()> {
        self.pending_relative = [0; 2];
        self.relative_dirty = false;
        let mut first_error = None;
        for (pressed, (_, usage)) in self.keys.iter_mut().zip(DESKTOP_KEY_USAGES.iter()) {
            if *pressed && let Some((virtual_key, extended)) = virtual_key_for_usage(*usage) {
                let flags = KEYEVENTF_KEYUP | if extended { KEYEVENTF_EXTENDEDKEY } else { 0 };
                match send_keyboard(virtual_key, flags) {
                    Ok(()) => *pressed = false,
                    Err(error) if first_error.is_none() => first_error = Some(error),
                    Err(_) => {}
                }
            }
        }
        for (bit, flag) in [(1, MOUSEEVENTF_LEFTUP), (2, MOUSEEVENTF_RIGHTUP)] {
            if self.mouse_buttons & bit != 0 {
                match send_mouse_button(flag) {
                    Ok(()) => self.mouse_buttons &= !bit,
                    Err(error) if first_error.is_none() => first_error = Some(error),
                    Err(_) => {}
                }
            }
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }
}

fn virtual_key_for_usage(usage: u8) -> Option<(u16, bool)> {
    match usage {
        0x28 => Some((0x0d, false)), // VK_RETURN
        0x29 => Some((0x1b, false)), // VK_ESCAPE
        0xe3 => Some((0x5b, true)),  // VK_LWIN
        0x4b => Some((0x21, true)),  // VK_PRIOR
        0x4e => Some((0x22, true)),  // VK_NEXT
        _ => None,
    }
}

fn send_keyboard(virtual_key: u16, flags: u32) -> io::Result<()> {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: virtual_key,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send_input(&input)
}

fn send_mouse_button(flags: u32) -> io::Result<()> {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send_input(&input)
}

fn send_relative_mouse(dx: i32, dy: i32) -> io::Result<()> {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send_input(&input)
}

fn send_input(input: &INPUT) -> io::Result<()> {
    let inserted = unsafe { SendInput(1, input, size_of::<INPUT>() as i32) };
    if inserted == 1 {
        Ok(())
    } else {
        // SendInput does not reliably identify UIPI as the reason for a zero
        // insertion count, so report failure without guessing its cause.
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Windows SendInput inserted no desktop input",
        ))
    }
}

struct OwnedToken(HANDLE);

impl Drop for OwnedToken {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn ensure_safe_desktop_token() -> io::Result<()> {
    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if token.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "OpenProcessToken returned a null token",
        ));
    }
    let token = OwnedToken(token);
    let elevation: TOKEN_ELEVATION = token_value(token.0, TokenElevation)?;
    let ui_access: u32 = token_value(token.0, TokenUIAccess)?;
    let session_id: u32 = token_value(token.0, TokenSessionId)?;
    if elevation.TokenIsElevated != 0 {
        return Err(permission_denied(
            "desktop output is disabled for elevated processes",
        ));
    }
    if ui_access != 0 {
        return Err(permission_denied(
            "desktop output is disabled for UIAccess processes",
        ));
    }
    if session_id == 0 {
        return Err(permission_denied(
            "desktop output requires an interactive user session",
        ));
    }
    if token_integrity_rid(token.0)? >= SECURITY_MANDATORY_HIGH_RID_VALUE {
        return Err(permission_denied(
            "desktop output is disabled at high integrity",
        ));
    }
    Ok(())
}

fn token_value<T: Copy>(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<T> {
    let mut value = MaybeUninit::<T>::uninit();
    let mut returned = 0u32;
    if unsafe {
        GetTokenInformation(
            token,
            class,
            value.as_mut_ptr().cast(),
            size_of::<T>() as u32,
            &mut returned,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if returned as usize != size_of::<T>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a malformed token information value",
        ));
    }
    Ok(unsafe { value.assume_init() })
}

fn token_integrity_rid(token: HANDLE) -> io::Result<u32> {
    let mut storage = [0usize; 64];
    let mut returned = 0u32;
    if unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            storage.as_mut_ptr().cast(),
            size_of::<[usize; 64]>() as u32,
            &mut returned,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if (returned as usize) < size_of::<TOKEN_MANDATORY_LABEL>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a truncated token integrity label",
        ));
    }
    let label = unsafe { &*storage.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
    let sid = label.Label.Sid;
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned an invalid token integrity SID",
        ));
    }
    let count = unsafe { GetSidSubAuthorityCount(sid) };
    if count.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a malformed token integrity SID",
        ));
    }
    let count = unsafe { *count };
    if count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned an empty token integrity SID",
        ));
    }
    let rid = unsafe { GetSidSubAuthority(sid, u32::from(count - 1)) };
    if rid.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a truncated token integrity SID",
        ));
    }
    Ok(unsafe { *rid })
}

fn permission_denied(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
fn output_errno(error: &io::Error) -> i32 {
    if let Some(code) = error.raw_os_error()
        && code != 0
    {
        return code.saturating_abs().saturating_neg();
    }
    match error.kind() {
        io::ErrorKind::PermissionDenied => -libc::EACCES,
        io::ErrorKind::InvalidInput => -libc::EINVAL,
        _ => -libc::EIO,
    }
}
fn no_output_sink() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "engine action targets an inactive Windows output sink",
    )
}
fn check_stopping(stop: Option<&AtomicBool>) -> io::Result<()> {
    if stop.is_some_and(|stop| stop.load(Ordering::Acquire)) {
        Err(io::Error::from_raw_os_error(libc::ECANCELED))
    } else {
        Ok(())
    }
}

fn is_stop_cancellation(error: &io::Error, stop: Option<&AtomicBool>) -> bool {
    error.raw_os_error() == Some(libc::ECANCELED)
        && stop.is_some_and(|stop| stop.load(Ordering::Acquire))
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Windows-owned equivalent of the Linux `BridgeDevice`; output ownership is
/// isolated from the Linux uinput backend and exposed for the Windows reactor.
pub struct WindowsBridgeDevice {
    syspath: PathBuf,
    profile: Profile,
    iface: Interface,
    engine: DeviceEngine,
    actions: Vec<OutputAction>,
    opened_ifaces: InterfaceMask,
    pending_ifaces: InterfaceMask,
    trace: Option<TraceContext>,
    config: Config,
    output: Option<OutputWorker>,
    capture: bool,
}

impl WindowsBridgeDevice {
    pub(crate) fn new_with_output_worker_permit(
        path: impl AsRef<Path>,
        config: &Config,
        outputs: bool,
        worker_permit_available: bool,
    ) -> Result<Self, i32> {
        let validated = config.clone().try_into().map_err(|_| -libc::EINVAL)?;
        let mut iface = Interface::new(path.as_ref()).map_err(|error| io_errno(&error))?;
        let syspath = iface.syspath().to_path_buf();
        let profile = profile_for_device(config, &syspath, None);
        let requested = requested_interfaces(profile, config);
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
        let output = if outputs {
            let gamepad = needs_gamepad(profile, config);
            let desktop = needs_desktop(profile, config);
            if gamepad || desktop {
                if !worker_permit_available {
                    return Err(-libc::ENOSPC);
                }
                Some(OutputWorker::spawn(gamepad, desktop)?)
            } else {
                None
            }
        } else {
            None
        };
        Ok(Self {
            syspath,
            profile,
            iface,
            engine: DeviceEngine::new(validated, profile),
            actions: Vec::with_capacity(32),
            opened_ifaces: opened,
            pending_ifaces: requested & !opened,
            trace: None,
            config: config.clone(),
            output,
            capture: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.syspath
    }

    pub fn wait_handle(&self) -> HANDLE {
        self.iface.wait_handle()
    }

    pub fn info(&self) -> DeviceInfo {
        let output_ready = self
            .output
            .as_ref()
            .is_some_and(|output| output.status() == OutputWorkerStatus::Ready);
        DeviceInfo {
            syspath: self.syspath.to_string_lossy().into_owned(),
            profile: ipc_profile(self.profile),
            opened_interfaces: self.opened_ifaces.bits(),
            pending_interfaces: self.pending_ifaces.bits(),
            gamepad_output: output_ready && needs_gamepad(self.profile, &self.config),
            desktop_output: output_ready && needs_desktop(self.profile, &self.config),
        }
    }

    pub fn set_capture(&mut self, enabled: bool) -> Result<(), i32> {
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

    fn requested_interfaces(&self) -> InterfaceMask {
        if self.capture {
            InterfaceMask::ALL
        } else {
            requested_interfaces(self.profile, &self.config)
        }
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
        self.pending_ifaces &= !opened;
        if !(opened & self.requested_interfaces()).is_empty() {
            return Ok(());
        }
        match result {
            Ok(_) => Err(-libc::ENODEV),
            Err(error) => Err(io_errno(error.source())),
        }
    }

    fn handle_watch(&mut self) -> Result<(), i32> {
        let opened = self.iface.opened();
        let lost = self.opened_ifaces & !opened & requested_interfaces(self.profile, &self.config);
        if !lost.is_empty() {
            self.reset()?;
        }
        self.opened_ifaces = opened;
        self.pending_ifaces = self.requested_interfaces() & !opened;
        self.retry_open()
    }

    pub fn drain(&mut self, observer: &mut dyn FnMut(&Event)) -> Result<BridgeAction, i32> {
        for _ in 0..super::MAX_EVENTS_PER_DRAIN {
            match self.iface.dispatch() {
                Ok(event) => {
                    if let Some(trace) = self.trace.as_mut() {
                        trace.emit(&self.syspath, &event);
                    }
                    observer(&event);
                    match self.handle_event(&event)? {
                        BridgeAction::Continue => {}
                        BridgeAction::Gone => return Ok(BridgeAction::Gone),
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(BridgeAction::Continue);
                }
                Err(error) => return Err(io_errno(&error)),
            }
        }
        Ok(BridgeAction::Continue)
    }

    pub fn handle_event(&mut self, event: &Event) -> Result<BridgeAction, i32> {
        match event.kind {
            EventKind::Gone => {
                self.actions.clear();
                self.engine.process(EngineInput::Reset, &mut self.actions);
                self.actions.clear();
                return Ok(BridgeAction::Gone);
            }
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

    /// Enqueue a refresh; success means accepted by the worker, not broker ack.
    pub fn refresh(&mut self) -> Result<(), i32> {
        self.output
            .as_mut()
            .map_or(Ok(()), OutputWorker::try_refresh)
    }

    pub(crate) fn output_status(&self) -> Option<OutputWorkerStatus> {
        self.output.as_ref().map(OutputWorker::status)
    }

    pub(crate) fn has_output_worker(&self) -> bool {
        self.output.is_some()
    }

    /// Enqueue a reset barrier before clearing engine-held state; success means
    /// the worker accepted it, not that the broker acknowledged the reset.
    pub fn reset(&mut self) -> Result<(), i32> {
        if let Some(output) = &mut self.output {
            output.try_reset()?;
        }
        self.actions.clear();
        self.engine.process(EngineInput::Reset, &mut self.actions);
        self.actions.clear();
        Ok(())
    }

    pub(crate) fn request_output_close(&mut self) -> Option<JoinHandle<()>> {
        self.output.as_mut().and_then(OutputWorker::request_close)
    }

    pub(crate) fn set_trace_sink_with_sequence<F: FnMut(&str) + 'static>(
        &mut self,
        filter: TraceFilter,
        sequence: Rc<Cell<u64>>,
        sink: F,
    ) {
        self.trace = Some(TraceContext::new(filter, sequence, sink));
    }

    fn process(&mut self, input: EngineInput) -> Result<(), i32> {
        if input == EngineInput::Reset {
            return self.reset();
        }
        self.actions.clear();
        self.engine.process(input, &mut self.actions);
        if let Some(output) = &mut self.output {
            for action in self.actions.iter().copied() {
                output.try_action(action)?;
            }
        }
        Ok(())
    }
}

fn ipc_profile(profile: Profile) -> wiiland_ipc::Profile {
    if profile == Profile::GAMEPAD {
        wiiland_ipc::Profile::Gamepad
    } else if profile == Profile::DESKTOP {
        wiiland_ipc::Profile::Desktop
    } else if profile == Profile::BOTH {
        wiiland_ipc::Profile::Both
    } else {
        wiiland_ipc::Profile::None
    }
}
