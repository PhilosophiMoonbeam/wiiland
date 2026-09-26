//! Windows HID backend for Nintendo Wii Remotes.
//!
//! Windows decodes Wii input/status/memory reports directly from the physical
//! HID collection and negotiates report modes for the requested sensor union.
#[path = "windows_protocol.rs"]
mod windows_protocol;

use crate::decode::{Event, EventKind, MotionPlusNormalizer};
use crate::model::{Axis3, Button, ButtonEvent, ButtonState, InterfaceMask, Timestamp};
use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HIDD_ATTRIBUTES, HIDP_CAPS, HIDP_STATUS_SUCCESS, HidD_FreePreparsedData, HidD_GetAttributes,
    HidD_GetHidGuid, HidD_GetPreparsedData, HidP_GetCaps, PHIDP_PREPARSED_DATA,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_DEVICE_NOT_CONNECTED, ERROR_DEVICE_REMOVED, ERROR_INSUFFICIENT_BUFFER,
    ERROR_IO_PENDING, ERROR_NO_MORE_ITEMS, ERROR_NO_SUCH_DEVICE, ERROR_NOT_FOUND,
    ERROR_OPERATION_ABORTED, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, WaitForSingleObject,
};

const NINTENDO_VENDOR_ID: u16 = 0x057e;
const WIIMOTE_PRODUCT_IDS: [u16; 2] = [0x0306, 0x0330];
const VHF_TEST_VENDOR_ID: u16 = 0xffff;
const VHF_TEST_PRODUCT_ID: u16 = 0x0001;
const INVALID_DEVICE_INFO_SET: HDEVINFO = -1;
const BUTTON_QUEUE_CAPACITY: usize = 64;
const MEMORY_READ_CAPACITY: usize = 64;
const ACK_QUEUE_CAPACITY: usize = 32;

const BUTTON_MAP: [(u16, Button); 11] = [
    (1 << 0, Button::Left),
    (1 << 1, Button::Right),
    (1 << 2, Button::Down),
    (1 << 3, Button::Up),
    (1 << 4, Button::Plus),
    (1 << 5, Button::Two),
    (1 << 6, Button::One),
    (1 << 7, Button::B),
    (1 << 8, Button::A),
    (1 << 9, Button::Minus),
    (1 << 10, Button::Home),
];

fn raw_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

fn last_error() -> io::Error {
    raw_error(unsafe { GetLastError() })
}

fn unsupported(feature: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, feature)
}

fn timestamp_now() -> Timestamp {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Timestamp {
        seconds: elapsed.as_secs() as i64,
        microseconds: elapsed.subsec_micros(),
    }
}

fn is_wiimote_identity(vendor_id: u16, product_id: u16) -> bool {
    if vendor_id == VHF_TEST_VENDOR_ID && product_id == VHF_TEST_PRODUCT_ID {
        return false;
    }
    vendor_id == NINTENDO_VENDOR_ID && WIIMOTE_PRODUCT_IDS.contains(&product_id)
}

struct Handle(HANDLE);

impl Handle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct DeviceInfoSet(HDEVINFO);

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        if self.0 != INVALID_DEVICE_INFO_SET {
            unsafe { SetupDiDestroyDeviceInfoList(self.0) };
        }
    }
}

struct PreparsedData(PHIDP_PREPARSED_DATA);

impl Drop for PreparsedData {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { HidD_FreePreparsedData(self.0) };
        }
    }
}

fn create_file(path: &Path, access: u32, flags: u32) -> io::Result<Handle> {
    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            flags,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(last_error())
    } else {
        Ok(Handle(handle))
    }
}

fn device_attributes(handle: HANDLE) -> io::Result<HIDD_ATTRIBUTES> {
    let mut attributes = HIDD_ATTRIBUTES {
        Size: std::mem::size_of::<HIDD_ATTRIBUTES>() as u32,
        ..HIDD_ATTRIBUTES::default()
    };
    if unsafe { HidD_GetAttributes(handle, &mut attributes) } {
        Ok(attributes)
    } else {
        Err(last_error())
    }
}

fn report_lengths(handle: HANDLE) -> io::Result<(usize, usize)> {
    let mut preparsed: PHIDP_PREPARSED_DATA = 0;
    if !unsafe { HidD_GetPreparsedData(handle, &mut preparsed) } {
        return Err(last_error());
    }
    let preparsed = PreparsedData(preparsed);
    let mut caps = HIDP_CAPS::default();
    let status = unsafe { HidP_GetCaps(preparsed.0, &mut caps) };
    if status != HIDP_STATUS_SUCCESS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("HidP_GetCaps returned NTSTATUS 0x{:08x}", status as u32),
        ));
    }
    let input = usize::from(caps.InputReportByteLength);
    let output = usize::from(caps.OutputReportByteLength);
    if input < 22 || output < 22 {
        return Err(unsupported(
            "Wii Remote HID descriptor lacks the full-size input or output reports",
        ));
    }
    Ok((input, output))
}

fn verify_wiimote(handle: HANDLE) -> io::Result<()> {
    let attributes = device_attributes(handle)?;
    if is_wiimote_identity(attributes.VendorID, attributes.ProductID) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "HID collection is not a supported Nintendo Wii Remote",
        ))
    }
}

enum ScanStep {
    End,
    Skip,
    Path(PathBuf),
}

struct MonitorScan {
    set: DeviceInfoSet,
    hid_guid: windows_sys::core::GUID,
    index: u32,
    paths: Vec<PathBuf>,
}

fn begin_monitor_scan() -> io::Result<MonitorScan> {
    let mut hid_guid = windows_sys::core::GUID {
        data1: 0,
        data2: 0,
        data3: 0,
        data4: [0; 8],
    };
    unsafe { HidD_GetHidGuid(&mut hid_guid) };
    let set = unsafe {
        SetupDiGetClassDevsW(
            &hid_guid,
            ptr::null(),
            ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if set == INVALID_DEVICE_INFO_SET {
        return Err(last_error());
    }
    Ok(MonitorScan {
        set: DeviceInfoSet(set),
        hid_guid,
        index: 0,
        paths: Vec::new(),
    })
}

fn monitor_scan_next(scan: &mut MonitorScan) -> io::Result<ScanStep> {
    let mut interface = SP_DEVICE_INTERFACE_DATA {
        cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
        ..SP_DEVICE_INTERFACE_DATA::default()
    };
    if unsafe {
        SetupDiEnumDeviceInterfaces(
            scan.set.0,
            ptr::null(),
            &scan.hid_guid,
            scan.index,
            &mut interface,
        )
    } == 0
    {
        let code = unsafe { GetLastError() };
        if code == ERROR_NO_MORE_ITEMS {
            return Ok(ScanStep::End);
        }
        return Err(raw_error(code));
    }
    scan.index = scan.index.checked_add(1).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "HID interface index overflow")
    })?;

    let mut required = 0u32;
    let detail_ok = unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            scan.set.0,
            &interface,
            ptr::null_mut(),
            0,
            &mut required,
            ptr::null_mut(),
        )
    };
    if detail_ok != 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
        return Err(last_error());
    }
    let detail_offset = std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
    let detail_bytes = usize::try_from(required)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "HID path size overflow"))?;
    if detail_bytes < detail_offset + std::mem::size_of::<u16>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a truncated HID interface path",
        ));
    }
    let words = detail_bytes.div_ceil(std::mem::size_of::<usize>());
    let mut storage = vec![0usize; words];
    let detail = storage
        .as_mut_ptr()
        .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
    unsafe {
        (*detail).cbSize = if std::mem::size_of::<usize>() == 8 {
            8
        } else {
            6
        };
    }
    if unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            scan.set.0,
            &interface,
            detail,
            required,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error());
    }
    let path_words = (detail_bytes - detail_offset) / std::mem::size_of::<u16>();
    let path_ptr = unsafe { (*detail).DevicePath.as_ptr() };
    let wide_path = unsafe { std::slice::from_raw_parts(path_ptr, path_words) };
    let path_len = wide_path
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(wide_path.len());
    if path_len == 0 {
        return Ok(ScanStep::Skip);
    }
    let path = PathBuf::from(OsString::from_wide(&wide_path[..path_len]));

    // HIDClass enumeration includes software VHF collections. Verify the
    // actual collection VID/PID before exposing a path.
    let Ok(probe) = create_file(&path, 0, FILE_ATTRIBUTE_NORMAL) else {
        return Ok(ScanStep::Skip);
    };
    if verify_wiimote(probe.raw()).is_ok() {
        Ok(ScanStep::Path(path))
    } else {
        Ok(ScanStep::Skip)
    }
}

/// Selects whether a monitor enumerates existing devices or watches for new ones.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MonitorMode {
    Enumerate,
    Watch,
}

/// Result of a bounded discovery pass.
#[derive(Debug, Eq, PartialEq)]
pub enum MonitorPoll {
    Path(PathBuf),
    /// The work budget was exhausted; call again even if no path was returned.
    Pending,
    /// Enumeration or the current watch snapshot is drained.
    Empty,
}

/// Enumerates Nintendo Wii Remote HID collection paths.
pub struct Monitor {
    mode: MonitorMode,
    pending: VecDeque<PathBuf>,
    known: HashSet<String>,
    scan: Option<MonitorScan>,
    enumeration_done: bool,
}

impl Monitor {
    pub fn new(mode: MonitorMode) -> io::Result<Self> {
        Ok(Self {
            mode,
            pending: VecDeque::new(),
            known: HashSet::new(),
            scan: None,
            enumeration_done: false,
        })
    }

    fn finish_scan(&mut self) -> MonitorPoll {
        let scan = self.scan.take().unwrap();
        if self.mode == MonitorMode::Enumerate {
            self.enumeration_done = true;
            return MonitorPoll::Empty;
        }
        let current = scan.paths;
        let current_keys: HashSet<String> = current.iter().map(|path| path_key(path)).collect();
        let previous = std::mem::replace(&mut self.known, current_keys);
        self.pending = current
            .into_iter()
            .filter(|path| !previous.contains(&path_key(path)))
            .collect();
        self.pending
            .pop_front()
            .map_or(MonitorPoll::Empty, MonitorPoll::Path)
    }

    /// Scans at most `budget` HID interface records; a zero budget returns
    /// `Pending`. At most one path is returned per call.
    pub fn poll_bounded(&mut self, budget: usize) -> io::Result<MonitorPoll> {
        if budget == 0 {
            return Ok(MonitorPoll::Pending);
        }
        if let Some(path) = self.pending.pop_front() {
            return Ok(MonitorPoll::Path(path));
        }
        if self.mode == MonitorMode::Enumerate && self.enumeration_done {
            return Ok(MonitorPoll::Empty);
        }
        if self.scan.is_none() {
            self.scan = Some(begin_monitor_scan()?);
        }
        for _ in 0..budget {
            match monitor_scan_next(self.scan.as_mut().unwrap())? {
                ScanStep::End => return Ok(self.finish_scan()),
                ScanStep::Skip => {}
                ScanStep::Path(path) if self.mode == MonitorMode::Enumerate => {
                    return Ok(MonitorPoll::Path(path));
                }
                ScanStep::Path(path) => self.scan.as_mut().unwrap().paths.push(path),
            }
        }
        Ok(MonitorPoll::Pending)
    }

    pub fn poll(&mut self) -> io::Result<Option<PathBuf>> {
        loop {
            match self.poll_bounded(64)? {
                MonitorPoll::Path(path) => return Ok(Some(path)),
                MonitorPoll::Pending => continue,
                MonitorPoll::Empty => return Ok(None),
            }
        }
    }
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_ascii_lowercase()
}

/// Error returned when opening one or more requested interfaces fails.
#[derive(Debug)]
pub struct OpenError {
    opened: InterfaceMask,
    error: io::Error,
}

impl OpenError {
    pub fn opened(&self) -> InterfaceMask {
        self.opened
    }

    pub fn source(&self) -> &io::Error {
        &self.error
    }
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (opened: 0x{:x})", self.error, self.opened.bits())
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}
#[derive(Clone, Copy)]
enum MemoryPurpose {
    AccelCalibration,
    ExtensionId,
    MotionPlusId,
    BalanceCalibration,
}

struct MemoryRead {
    purpose: MemoryPurpose,
    address: u32,
    total: usize,
    received: usize,
    bytes: [u8; MEMORY_READ_CAPACITY],
}

#[derive(Clone, Copy)]
struct PendingAck {
    report_id: u8,
    motion_plus_mode: Option<u8>,
}

/// One physical Wii Remote HID collection.
///
/// Windows timestamps are assigned when `ReadFile` completes; HID input reports
/// do not carry the Linux evdev timeval used by the Linux backend.
pub struct Interface {
    device: Option<Handle>,
    read_event: Handle,
    write_event: Handle,
    read_overlapped: Box<OVERLAPPED>,
    read_pending: bool,
    path: PathBuf,
    input_len: usize,
    output_len: usize,
    input_report: Vec<u8>,
    output_report: RefCell<Vec<u8>>,
    opened: InterfaceMask,
    available: InterfaceMask,
    buttons: u16,
    buttons_seeded: bool,
    extension_buttons: u32,
    extension_buttons_seeded: bool,
    extension_kind: windows_protocol::ExtensionKind,
    extension_connected: Option<bool>,
    motion_plus_present: bool,
    motion_plus_initialized: bool,
    motion_plus_active: bool,
    motion_plus_mode: Option<u8>,
    motion_plus_extension_connected: Option<bool>,
    current_mode: Option<u8>,
    report_plan: Option<windows_protocol::ModePlan>,
    memory_read: Option<MemoryRead>,
    accel_calibration: Option<windows_protocol::AccelCalibration>,
    balance_calibration: Option<windows_protocol::BalanceCalibration>,
    reinitialize_after_memory: bool,
    discovery_started: bool,
    pending_acks: [Option<PendingAck>; ACK_QUEUE_CAPACITY],
    pending: [Option<Event>; BUTTON_QUEUE_CAPACITY],
    pending_pos: usize,
    pending_len: usize,
    rumble_on: Cell<bool>,
    battery: Cell<Option<u8>>,
    leds: Cell<Option<u8>>,
    ir_initialized: bool,
    ir_camera_mode: Option<u8>,
    watch_enabled: bool,
    gone: bool,
    gone_reported: bool,
    motion_plus: MotionPlusNormalizer,
    motion_plus_offset_remainder: Axis3,
}

impl Interface {
    pub fn new(path: &Path) -> io::Result<Self> {
        let device = create_file(
            path,
            GENERIC_READ | GENERIC_WRITE,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
        )?;
        verify_wiimote(device.raw())?;
        let (input_len, output_len) = report_lengths(device.raw())?;
        drop(device);

        let read_event = create_event()?;
        let write_event = create_event()?;
        Ok(Self {
            device: None,
            read_event,
            write_event,
            read_overlapped: Box::new(OVERLAPPED::default()),
            read_pending: false,
            path: path.to_path_buf(),
            input_len,
            input_report: vec![0; input_len],
            output_len,
            output_report: RefCell::new(vec![0; output_len]),
            opened: InterfaceMask::empty(),
            available: InterfaceMask::CORE | InterfaceMask::ACCEL | InterfaceMask::IR,
            buttons: 0,
            buttons_seeded: false,
            extension_buttons: 0,
            extension_buttons_seeded: false,
            extension_kind: windows_protocol::ExtensionKind::None,
            extension_connected: None,
            motion_plus_present: false,
            motion_plus_initialized: false,
            motion_plus_active: false,
            motion_plus_mode: None,
            motion_plus_extension_connected: None,
            current_mode: None,
            report_plan: None,
            memory_read: None,
            accel_calibration: None,
            balance_calibration: None,
            reinitialize_after_memory: false,
            discovery_started: false,
            pending_acks: [None; ACK_QUEUE_CAPACITY],
            pending: [None; BUTTON_QUEUE_CAPACITY],
            pending_pos: 0,
            pending_len: 0,
            rumble_on: Cell::new(false),
            battery: Cell::new(None),
            leds: Cell::new(None),
            ir_initialized: false,
            ir_camera_mode: None,
            watch_enabled: false,
            gone: false,
            gone_reported: false,
            motion_plus: MotionPlusNormalizer::new(),
            motion_plus_offset_remainder: Axis3::default(),
        })
    }
    /// Returns the stable manual-reset event used by the overlapped input read.
    ///
    /// The handle remains valid while this `Interface` is alive and must not be
    /// closed by the caller. It is signaled when the current read completes;
    /// call `dispatch()` after waking so the completion is consumed and the next
    /// read is armed. With no read armed, the event is unsignaled.
    pub fn wait_handle(&self) -> HANDLE {
        self.read_event.raw()
    }

    pub fn syspath(&self) -> &Path {
        &self.path
    }

    fn ensure_device(&mut self) -> io::Result<()> {
        if self.device.is_some() {
            return Ok(());
        }
        let device = create_file(
            &self.path,
            GENERIC_READ | GENERIC_WRITE,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
        )?;
        verify_wiimote(device.raw())?;
        let (input_len, output_len) = report_lengths(device.raw())?;
        if input_len != self.input_len || output_len != self.output_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Wii Remote HID report sizes changed after discovery",
            ));
        }
        self.device = Some(device);
        Ok(())
    }

    fn device_handle(&self) -> io::Result<HANDLE> {
        self.device
            .as_ref()
            .map(Handle::raw)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "Wii Remote is closed"))
    }

    pub fn opened(&self) -> InterfaceMask {
        self.opened
    }

    pub fn available(&self) -> InterfaceMask {
        self.available
    }

    fn make_overlapped(event: HANDLE) -> OVERLAPPED {
        OVERLAPPED {
            hEvent: event,
            ..OVERLAPPED::default()
        }
    }

    fn write_report_packet(&self, packet: &[u8]) -> io::Result<()> {
        let handle = self.device_handle()?;
        let mut output = self.output_report.borrow_mut();
        if packet.len() > output.len() || output.len() < 22 {
            return Err(unsupported("Wii Remote output report is too short"));
        }
        output.fill(0);
        output[..packet.len()].copy_from_slice(packet);
        if unsafe { ResetEvent(self.write_event.raw()) } == 0 {
            return Err(last_error());
        }
        let mut overlapped = Self::make_overlapped(self.write_event.raw());
        let report_len = output.len();
        let write_ok = unsafe {
            WriteFile(
                handle,
                output.as_ptr(),
                report_len as u32,
                ptr::null_mut(),
                &mut overlapped,
            )
        };
        if write_ok == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
            return Err(last_error());
        }
        let mut transferred = 0u32;
        if unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 1) } == 0 {
            return Err(last_error());
        }
        if transferred as usize != report_len {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short Wii Remote HID output report",
            ));
        }
        Ok(())
    }

    fn write_packet_ack(&mut self, packet: &[u8], activates_motion_plus: bool) -> io::Result<()> {
        let ack_index = if packet
            .get(1)
            .is_some_and(|flags| flags & windows_protocol::REPORT_OUTPUT_COMMON_ACK != 0)
        {
            let index = self
                .pending_acks
                .iter()
                .position(Option::is_none)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "too many unacknowledged Wii Remote output reports",
                    )
                })?;
            self.pending_acks[index] = Some(PendingAck {
                report_id: packet[0],
                motion_plus_mode: if activates_motion_plus {
                    packet.get(6).copied()
                } else {
                    None
                },
            });
            Some(index)
        } else {
            None
        };
        if let Err(error) = self.write_report_packet(packet) {
            if let Some(index) = ack_index {
                self.pending_acks[index] = None;
            }
            return Err(error);
        }
        Ok(())
    }

    fn write_report_mode(&mut self, mode: u8) -> io::Result<()> {
        let packet = [
            windows_protocol::REPORT_MODE_OUTPUT,
            u8::from(self.rumble_on.get()) | windows_protocol::REPORT_OUTPUT_COMMON_ACK,
            mode,
        ];
        self.write_packet_ack(&packet, false)?;
        self.current_mode = Some(mode);
        Ok(())
    }

    fn write_report_flags(&mut self, report_id: u8, enable: bool) -> io::Result<()> {
        let packet = [
            report_id,
            u8::from(self.rumble_on.get())
                | windows_protocol::REPORT_OUTPUT_COMMON_ACK
                | if enable {
                    windows_protocol::REPORT_OUTPUT_COMMON_ENABLE
                } else {
                    0
                },
        ];
        self.write_packet_ack(&packet, false)
    }

    fn start_read(&mut self) -> io::Result<()> {
        if self.read_pending {
            return Ok(());
        }
        let handle = self.device_handle()?;
        if unsafe { ResetEvent(self.read_event.raw()) } == 0 {
            return Err(last_error());
        }
        *self.read_overlapped = Self::make_overlapped(self.read_event.raw());
        let read_ok = unsafe {
            ReadFile(
                handle,
                self.input_report.as_mut_ptr(),
                self.input_report.len() as u32,
                ptr::null_mut(),
                &mut *self.read_overlapped,
            )
        };
        if read_ok == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
            return Err(last_error());
        }
        self.read_pending = true;
        Ok(())
    }

    fn cancel_read(&mut self) {
        if !self.read_pending {
            return;
        }
        if let Ok(handle) = self.device_handle() {
            if unsafe { CancelIoEx(handle, &*self.read_overlapped) } == 0
                && unsafe { GetLastError() } != ERROR_NOT_FOUND
            {
                // Closing the device handle cancels an I/O that could not be
                // cancelled by its OVERLAPPED address.
                self.device = None;
            }
            unsafe { WaitForSingleObject(self.read_event.raw(), INFINITE) };
            if self.device.is_some() {
                let mut bytes = 0u32;
                unsafe { GetOverlappedResult(handle, &*self.read_overlapped, &mut bytes, 0) };
            }
        }
        self.read_pending = false;
        *self.read_overlapped = OVERLAPPED::default();
        unsafe { ResetEvent(self.read_event.raw()) };
    }

    fn mark_gone(&mut self) {
        self.cancel_read();
        self.device = None;
        self.available = InterfaceMask::empty();
        self.memory_read = None;
        self.pending_acks = [None; ACK_QUEUE_CAPACITY];
        self.pending = [None; BUTTON_QUEUE_CAPACITY];
        self.pending_pos = 0;
        self.pending_len = 0;
        self.battery.set(None);
        self.leds.set(None);
        self.gone = true;
        self.gone_reported = false;
    }

    fn disconnected_error(error: &io::Error) -> bool {
        matches!(
            error.raw_os_error().map(|code| code as u32),
            Some(ERROR_DEVICE_NOT_CONNECTED)
                | Some(ERROR_DEVICE_REMOVED)
                | Some(ERROR_NO_SUCH_DEVICE)
                | Some(ERROR_OPERATION_ABORTED)
        )
    }

    fn enqueue(&mut self, event: Event) {
        debug_assert!(self.pending_len < BUTTON_QUEUE_CAPACITY);
        if self.pending_len < BUTTON_QUEUE_CAPACITY {
            let index = (self.pending_pos + self.pending_len) % BUTTON_QUEUE_CAPACITY;
            self.pending[index] = Some(event);
            self.pending_len += 1;
        }
    }

    fn dequeue(&mut self) -> Option<Event> {
        if self.pending_len == 0 {
            return None;
        }
        let event = self.pending[self.pending_pos].take();
        self.pending_pos = (self.pending_pos + 1) % BUTTON_QUEUE_CAPACITY;
        self.pending_len -= 1;
        if self.pending_len == 0 {
            self.pending_pos = 0;
        }
        event
    }

    fn update_button_bits(&mut self, current_buttons: u16, time: Timestamp) {
        if self.opened.contains(InterfaceMask::CORE) {
            if self.buttons_seeded {
                for (mask, button) in BUTTON_MAP {
                    if (self.buttons ^ current_buttons) & mask != 0 {
                        let state = if current_buttons & mask != 0 {
                            ButtonState::Pressed
                        } else {
                            ButtonState::Released
                        };
                        self.enqueue(Event {
                            time,
                            kind: EventKind::Key(ButtonEvent { button, state }),
                        });
                    }
                }
            }
            self.buttons = current_buttons;
            self.buttons_seeded = true;
        } else {
            self.buttons = current_buttons;
            self.buttons_seeded = false;
        }
    }

    fn update_available(&mut self) {
        let mut available = InterfaceMask::CORE | InterfaceMask::ACCEL | InterfaceMask::IR;
        if let Some(extension) = self.extension_kind.interface() {
            available |= extension;
        }
        let passthrough_supported = matches!(
            self.extension_kind,
            windows_protocol::ExtensionKind::None
                | windows_protocol::ExtensionKind::Nunchuk
                | windows_protocol::ExtensionKind::Classic
                | windows_protocol::ExtensionKind::Pro
                | windows_protocol::ExtensionKind::Guitar
                | windows_protocol::ExtensionKind::Drums
                | windows_protocol::ExtensionKind::MotionPlus
        );
        if self.motion_plus_present && passthrough_supported {
            available |= InterfaceMask::MOTION_PLUS;
        }
        self.available = available;
    }

    fn configure_report_mode(&mut self, force: bool) -> io::Result<()> {
        let plan = windows_protocol::report_modes(self.opened, self.extension_kind);
        let Some(plan) = plan else {
            self.current_mode = None;
            self.report_plan = None;
            return Ok(());
        };
        if force
            || self.report_plan != Some(plan)
            || self.current_mode.is_none_or(|mode| !plan.contains(mode))
        {
            self.write_report_mode(plan.first())?;
        }
        self.report_plan = Some(plan);
        Ok(())
    }

    fn update_extension_buttons(&mut self, current: u32, time: Timestamp) {
        let Some(interface) = self.extension_kind.interface() else {
            self.extension_buttons = 0;
            self.extension_buttons_seeded = false;
            return;
        };
        let map = self.extension_kind.button_map();
        if !self.opened.contains(interface) {
            self.extension_buttons = current;
            self.extension_buttons_seeded = false;
            return;
        }
        if self.extension_buttons_seeded {
            for (index, &button) in map.iter().enumerate() {
                let mask = 1u32 << index;
                if (self.extension_buttons ^ current) & mask != 0
                    && let Some(kind) = windows_protocol::button_event(
                        self.extension_kind,
                        button,
                        if current & mask != 0 {
                            ButtonState::Pressed
                        } else {
                            ButtonState::Released
                        },
                    )
                {
                    self.enqueue(Event { time, kind });
                }
            }
        }
        self.extension_buttons = current;
        self.extension_buttons_seeded = true;
    }

    fn replace_extension_kind(
        &mut self,
        extension: windows_protocol::ExtensionKind,
        time: Timestamp,
    ) -> io::Result<()> {
        if self.extension_kind == extension {
            return Ok(());
        }
        if self.extension_buttons_seeded {
            self.update_extension_buttons(0, time);
        }
        self.extension_kind = extension;
        self.extension_buttons = 0;
        self.extension_buttons_seeded = false;
        if extension != windows_protocol::ExtensionKind::BalanceBoard {
            self.balance_calibration = None;
        }
        if extension == windows_protocol::ExtensionKind::MotionPlus {
            self.motion_plus_present = true;
            self.motion_plus_active = false;
            self.motion_plus_mode = None;
        }
        let previous_available = self.available;
        self.update_available();
        if self.watch_enabled && self.available != previous_available {
            self.enqueue(Event {
                time,
                kind: EventKind::Watch,
            });
        }
        if self.opened.contains(InterfaceMask::IR) && self.ir_initialized {
            self.setup_ir_camera(true)?;
        }
        self.configure_report_mode(true)
    }

    fn write_memory_register(&mut self, address: u32, data: &[u8]) -> io::Result<()> {
        let packet =
            windows_protocol::memory_write_packet(address, data, true, self.rumble_on.get())?;
        self.write_packet_ack(&packet, false)
    }

    fn request_memory(
        &mut self,
        address: u32,
        length: usize,
        registers: bool,
        purpose: MemoryPurpose,
    ) -> io::Result<()> {
        if self.memory_read.is_some() || length == 0 || length > MEMORY_READ_CAPACITY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid or overlapping Wii Remote memory read",
            ));
        }
        let length = u16::try_from(length).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Wii Remote memory read is too large",
            )
        })?;
        let packet =
            windows_protocol::memory_read_packet(address, length, registers, self.rumble_on.get());
        self.write_report_packet(&packet)?;
        self.memory_read = Some(MemoryRead {
            purpose,
            address,
            total: usize::from(length),
            received: 0,
            bytes: [0; MEMORY_READ_CAPACITY],
        });
        Ok(())
    }

    fn request_status(&mut self) -> io::Result<()> {
        self.write_report_packet(&[
            windows_protocol::REPORT_STATUS_OUTPUT,
            u8::from(self.rumble_on.get()),
        ])
    }

    fn setup_ir_camera(&mut self, enable: bool) -> io::Result<()> {
        if !enable {
            self.ir_initialized = false;
            self.ir_camera_mode = None;
            self.write_report_flags(windows_protocol::REPORT_IR_ENABLE_1, false)?;
            std::thread::sleep(Duration::from_millis(50));
            self.write_report_flags(windows_protocol::REPORT_IR_ENABLE_2, false)?;
            std::thread::sleep(Duration::from_millis(50));
            return Ok(());
        }

        const SENSITIVITY_1: [u8; 9] = [0x02, 0, 0, 0x71, 0x01, 0, 0x64, 0, 0xfe];
        const SENSITIVITY_2: [u8; 2] = [0xfd, 0x05];
        let camera_mode = if windows_protocol::report_modes(self.opened, self.extension_kind)
            .is_some_and(|plan| plan.contains(0x33))
        {
            0x03
        } else {
            0x01
        };
        if self.ir_initialized && self.ir_camera_mode == Some(camera_mode) {
            return Ok(());
        }
        if self.ir_initialized {
            self.ir_initialized = false;
            self.ir_camera_mode = None;
            self.write_report_flags(windows_protocol::REPORT_IR_ENABLE_1, false)?;
            std::thread::sleep(Duration::from_millis(50));
            self.write_report_flags(windows_protocol::REPORT_IR_ENABLE_2, false)?;
            std::thread::sleep(Duration::from_millis(50));
        }
        self.write_report_flags(windows_protocol::REPORT_IR_ENABLE_1, true)?;
        self.ir_initialized = true;
        std::thread::sleep(Duration::from_millis(50));
        self.write_report_flags(windows_protocol::REPORT_IR_ENABLE_2, true)?;
        std::thread::sleep(Duration::from_millis(50));
        self.write_memory_register(windows_protocol::IR_REGISTER_BASE + 0x30, &[0x01])?;
        std::thread::sleep(Duration::from_millis(50));
        self.write_memory_register(windows_protocol::IR_REGISTER_BASE, &SENSITIVITY_1)?;
        std::thread::sleep(Duration::from_millis(50));
        self.write_memory_register(windows_protocol::IR_REGISTER_BASE + 0x1a, &SENSITIVITY_2)?;
        std::thread::sleep(Duration::from_millis(50));
        self.write_memory_register(windows_protocol::IR_REGISTER_BASE + 0x33, &[camera_mode])?;
        std::thread::sleep(Duration::from_millis(50));
        self.write_memory_register(windows_protocol::IR_REGISTER_BASE + 0x30, &[0x08])?;
        self.ir_initialized = true;
        self.ir_camera_mode = Some(camera_mode);
        Ok(())
    }

    fn start_extension_discovery(&mut self) -> io::Result<()> {
        self.balance_calibration = None;
        self.motion_plus_initialized = false;
        self.replace_extension_kind(windows_protocol::ExtensionKind::None, timestamp_now())?;
        if self.extension_connected == Some(true) {
            self.write_memory_register(windows_protocol::EXTENSION_REGISTER_BASE + 0xf0, &[0x55])?;
            self.write_memory_register(windows_protocol::EXTENSION_REGISTER_BASE + 0xfb, &[0x00])?;
            self.request_memory(
                windows_protocol::EXTENSION_REGISTER_BASE + 0xfa,
                6,
                true,
                MemoryPurpose::ExtensionId,
            )
        } else {
            self.start_motion_plus_probe()
        }
    }

    fn start_motion_plus_probe(&mut self) -> io::Result<()> {
        self.request_memory(
            windows_protocol::MOTION_PLUS_REGISTER_BASE + 0xfa,
            6,
            true,
            MemoryPurpose::MotionPlusId,
        )
    }

    fn activate_motion_plus(&mut self) -> io::Result<()> {
        let nunchuk_requested = self.opened.contains(InterfaceMask::NUNCHUK);
        let classic_or_instrument_requested =
            self.opened.contains(InterfaceMask::CLASSIC_CONTROLLER)
                || self.opened.contains(InterfaceMask::PRO_CONTROLLER)
                || self.opened.contains(InterfaceMask::GUITAR)
                || self.opened.contains(InterfaceMask::DRUMS);
        if !self.motion_plus_present
            || (!self.opened.contains(InterfaceMask::MOTION_PLUS)
                && !nunchuk_requested
                && !classic_or_instrument_requested)
        {
            return Ok(());
        }
        let mode = match self.extension_kind {
            windows_protocol::ExtensionKind::Nunchuk => 0x05,
            windows_protocol::ExtensionKind::Classic
            | windows_protocol::ExtensionKind::Pro
            | windows_protocol::ExtensionKind::Guitar
            | windows_protocol::ExtensionKind::Drums => 0x07,
            windows_protocol::ExtensionKind::None | windows_protocol::ExtensionKind::MotionPlus => {
                if nunchuk_requested {
                    0x05
                } else if classic_or_instrument_requested {
                    0x07
                } else if self.opened.contains(InterfaceMask::MOTION_PLUS) {
                    0x04
                } else {
                    return Ok(());
                }
            }
            windows_protocol::ExtensionKind::BalanceBoard
            | windows_protocol::ExtensionKind::Unknown => return Ok(()),
        };
        if self.motion_plus_active
            && self.motion_plus_mode == Some(mode)
            && self.motion_plus_initialized
        {
            return Ok(());
        }
        if !self.motion_plus_initialized {
            self.write_memory_register(
                windows_protocol::MOTION_PLUS_REGISTER_BASE + 0xf0,
                &[0x55],
            )?;
            self.motion_plus_initialized = true;
        }
        let packet = windows_protocol::memory_write_packet(
            windows_protocol::MOTION_PLUS_REGISTER_BASE + 0xfe,
            &[mode],
            true,
            self.rumble_on.get(),
        )?;
        self.write_packet_ack(&packet, true)
    }

    fn handle_status(&mut self, status: windows_protocol::StatusReport) -> io::Result<()> {
        let time = timestamp_now();
        self.update_button_bits(status.buttons, time);
        self.battery
            .set(Some((u16::from(status.battery) * 100 / 255) as u8));
        self.leds.set(Some(status.led_mask()));
        let connected = status.extension_connected();
        let virtual_motion_plus =
            self.motion_plus_active && connected && self.extension_connected == Some(false);
        let changed = self
            .extension_connected
            .is_some_and(|previous| previous != connected)
            && !virtual_motion_plus;
        if !virtual_motion_plus {
            self.extension_connected = Some(connected);
        }
        if changed {
            self.balance_calibration = None;
            self.motion_plus_initialized = false;
            self.motion_plus_active = false;
            self.motion_plus_mode = None;
            self.motion_plus_extension_connected = None;
            self.replace_extension_kind(windows_protocol::ExtensionKind::None, time)?;
        }
        if !self.opened.is_empty() {
            self.configure_report_mode(true)?;
        }
        if !self.discovery_started {
            self.discovery_started = true;
            self.request_memory(
                windows_protocol::EEPROM_ACCEL_CALIBRATION,
                10,
                false,
                MemoryPurpose::AccelCalibration,
            )?;
        } else if changed {
            if self.memory_read.is_some() {
                self.reinitialize_after_memory = true;
            } else {
                self.start_extension_discovery()?;
            }
        }
        Ok(())
    }

    fn finish_memory_read(&mut self, purpose: MemoryPurpose, bytes: &[u8]) -> io::Result<()> {
        if self.reinitialize_after_memory {
            self.reinitialize_after_memory = false;
            return self.start_extension_discovery();
        }
        match purpose {
            MemoryPurpose::AccelCalibration => {
                self.accel_calibration = windows_protocol::AccelCalibration::from_eeprom(bytes);
                self.start_extension_discovery()
            }
            MemoryPurpose::ExtensionId => {
                let extension = windows_protocol::classify_extension_id(bytes);
                self.replace_extension_kind(extension, timestamp_now())?;
                match extension {
                    windows_protocol::ExtensionKind::MotionPlus => self.activate_motion_plus(),
                    windows_protocol::ExtensionKind::BalanceBoard => self.request_memory(
                        windows_protocol::EXTENSION_REGISTER_BASE + 0x24,
                        24,
                        true,
                        MemoryPurpose::BalanceCalibration,
                    ),
                    _ => self.start_motion_plus_probe(),
                }
            }
            MemoryPurpose::BalanceCalibration => {
                let bytes: [u8; 24] = bytes.try_into().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Wii Remote Balance Board calibration read has an invalid length",
                    )
                })?;
                self.balance_calibration =
                    Some(windows_protocol::BalanceCalibration::from_bytes(bytes));
                self.start_motion_plus_probe()
            }
            MemoryPurpose::MotionPlusId => {
                let previous_available = self.available;
                self.motion_plus_present = windows_protocol::is_motion_plus_id(bytes);
                if !self.motion_plus_present {
                    self.motion_plus_active = false;
                    self.motion_plus_mode = None;
                    self.motion_plus_initialized = false;
                }
                self.update_available();
                if self.watch_enabled && self.available != previous_available {
                    self.enqueue(Event {
                        time: timestamp_now(),
                        kind: EventKind::Watch,
                    });
                }
                self.activate_motion_plus()
            }
        }
    }

    fn handle_memory_report(&mut self, memory: windows_protocol::MemoryReport) -> io::Result<()> {
        let time = timestamp_now();
        self.update_button_bits(memory.buttons, time);
        let Some(mut read) = self.memory_read.take() else {
            return Ok(());
        };
        let expected = read.address.wrapping_add(read.received as u32) as u16;
        if memory.offset != expected {
            self.memory_read = Some(read);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Wii Remote memory response offset does not match the active read",
            ));
        }
        if memory.error != 0 {
            let purpose = read.purpose;
            if self.reinitialize_after_memory {
                self.reinitialize_after_memory = false;
                return self.start_extension_discovery();
            }
            match (purpose, memory.error) {
                (MemoryPurpose::AccelCalibration, 7 | 8) => {
                    self.accel_calibration = None;
                    return self.start_extension_discovery();
                }
                (MemoryPurpose::ExtensionId, 7) => {
                    self.replace_extension_kind(
                        if self.extension_connected == Some(true) {
                            windows_protocol::ExtensionKind::Unknown
                        } else {
                            windows_protocol::ExtensionKind::None
                        },
                        time,
                    )?;
                    return self.start_motion_plus_probe();
                }
                (MemoryPurpose::MotionPlusId, 7 | 8) => {
                    let previous_available = self.available;
                    self.motion_plus_present = false;
                    self.motion_plus_initialized = false;
                    self.motion_plus_active = false;
                    self.motion_plus_mode = None;
                    self.update_available();
                    if self.watch_enabled && self.available != previous_available {
                        self.enqueue(Event {
                            time,
                            kind: EventKind::Watch,
                        });
                    }
                    return Ok(());
                }
                (MemoryPurpose::BalanceCalibration, 7 | 8) => {
                    self.balance_calibration = None;
                    return self.start_motion_plus_probe();
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "Wii Remote memory read at 0x{:06x} failed with protocol error {}",
                            read.address, memory.error
                        ),
                    ));
                }
            }
        }
        let remaining = read.total - read.received;
        if memory.len == 0 || memory.len > remaining {
            self.memory_read = Some(read);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Wii Remote memory response length exceeds the requested range",
            ));
        }
        let start = read.received;
        let end = start + memory.len;
        read.bytes[start..end].copy_from_slice(&memory.bytes[..memory.len]);
        read.received = end;
        if read.received == read.total {
            self.finish_memory_read(read.purpose, &read.bytes[..read.total])
        } else {
            self.memory_read = Some(read);
            Ok(())
        }
    }

    fn handle_ack(&mut self, ack: windows_protocol::AckReport) -> io::Result<()> {
        self.update_button_bits(ack.buttons, timestamp_now());
        let Some(index) = self.pending_acks.iter().position(|pending| {
            pending
                .as_ref()
                .is_some_and(|pending| pending.report_id == ack.report_id)
        }) else {
            return Ok(());
        };
        let pending = self.pending_acks[index].take().unwrap();
        if ack.error != 0 {
            if pending.motion_plus_mode.is_some() {
                self.motion_plus_active = false;
                self.motion_plus_mode = None;
                self.update_available();
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Wii Remote output report 0x{:02x} failed with protocol error {}",
                    ack.report_id, ack.error
                ),
            ));
        }
        if let Some(mode) = pending.motion_plus_mode {
            self.motion_plus_active = true;
            self.motion_plus_mode = Some(mode);
            self.update_available();
            self.configure_report_mode(true)?;
        }
        Ok(())
    }

    fn calibrated_accel(&self, mut sample: Axis3) -> Axis3 {
        if let Some(calibration) = self.accel_calibration {
            sample.x += 0x200 - calibration.zero.x;
            sample.y += 0x200 - calibration.zero.y;
            sample.z += 0x200 - calibration.zero.z;
        }
        sample
    }

    fn normalize_motion_plus(&mut self, sample: Axis3) -> Axis3 {
        let (mut x_offset, mut y_offset, mut z_offset, factor) = self.motion_plus.values();
        let mut remainder = self.motion_plus_offset_remainder;
        let normalize = |value: i32, offset: &mut i32, remainder: &mut i32| {
            let mut scaled_offset = i64::from(*offset) * 100 + i64::from(*remainder);
            let normalized = (i64::from(value) - scaled_offset / 100)
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX));
            if normalized != 0 {
                scaled_offset = (scaled_offset
                    + if normalized > 0 {
                        i64::from(factor)
                    } else {
                        -i64::from(factor)
                    })
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX));
            }
            let whole = scaled_offset / 100;
            *offset = whole as i32;
            *remainder = (scaled_offset - whole * 100) as i32;
            normalized as i32
        };
        let normalized = Axis3 {
            x: normalize(sample.x, &mut x_offset, &mut remainder.x),
            y: normalize(sample.y, &mut y_offset, &mut remainder.y),
            z: normalize(sample.z, &mut z_offset, &mut remainder.z),
        };
        self.motion_plus.set(x_offset, y_offset, z_offset, factor);
        self.motion_plus_offset_remainder = remainder;
        normalized
    }

    fn handle_motion_plus_attachment(&mut self, connected: bool) -> io::Result<()> {
        let changed = match self.motion_plus_extension_connected {
            Some(previous) => previous != connected,
            None => connected,
        };
        self.motion_plus_extension_connected = Some(connected);
        if !changed {
            return Ok(());
        }
        self.balance_calibration = None;
        self.motion_plus_initialized = false;
        self.extension_connected = Some(connected);
        if self.memory_read.is_some() {
            self.reinitialize_after_memory = true;
            Ok(())
        } else {
            self.start_extension_discovery()
        }
    }

    fn handle_data_frame(&mut self, frame: windows_protocol::DataFrame) -> io::Result<()> {
        let time = timestamp_now();
        if let Some(buttons) = frame.buttons {
            self.update_button_bits(buttons, time);
        }
        if self.opened.contains(InterfaceMask::ACCEL)
            && let Some(accel) = frame.accel
        {
            self.enqueue(Event {
                time,
                kind: EventKind::Accel(self.calibrated_accel(accel)),
            });
        }
        if self.opened.contains(InterfaceMask::IR)
            && let Some(ir) = frame.ir
        {
            self.enqueue(Event {
                time,
                kind: EventKind::Ir(ir),
            });
        }
        if let Some(extension) = frame.extension {
            let is_motion_plus = extension.movement.interface() == Some(InterfaceMask::MOTION_PLUS);
            if !is_motion_plus {
                self.update_extension_buttons(extension.buttons, time);
            }
            if extension
                .movement
                .interface()
                .is_some_and(|interface| self.opened.contains(interface))
            {
                let kind = if let EventKind::MotionPlus(sample) = extension.movement {
                    EventKind::MotionPlus(self.normalize_motion_plus(sample))
                } else {
                    extension.movement
                };
                self.enqueue(Event { time, kind });
            }
        }
        if let Some(connected) = frame.motion_plus_extension_connected {
            self.handle_motion_plus_attachment(connected)?;
        }
        if let Some(current) = self.current_mode
            && let Some(next) = self.report_plan.and_then(|plan| plan.next_after(current))
        {
            self.write_report_mode(next)?;
        }
        Ok(())
    }

    fn decode_report(&mut self, length: usize) -> io::Result<()> {
        if length == 0 || length > self.input_report.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Wii Remote HID input report length",
            ));
        }
        match self.input_report[0] {
            windows_protocol::REPORT_STATUS => {
                let status = windows_protocol::parse_status(&self.input_report[..length])?;
                self.start_read()?;
                self.handle_status(status)
            }
            windows_protocol::REPORT_MEMORY => {
                let memory = windows_protocol::parse_memory(&self.input_report[..length])?;
                self.start_read()?;
                self.handle_memory_report(memory)
            }
            windows_protocol::REPORT_ACK => {
                let ack = windows_protocol::parse_ack(&self.input_report[..length])?;
                self.start_read()?;
                self.handle_ack(ack)
            }
            _ => {
                let frame = windows_protocol::decode_data(
                    &self.input_report[..length],
                    self.extension_kind,
                    self.motion_plus_active,
                    self.balance_calibration.as_ref(),
                )?;
                self.start_read()?;
                if let Some(frame) = frame {
                    self.handle_data_frame(frame)
                } else {
                    Ok(())
                }
            }
        }
    }

    pub fn open(&mut self, ifaces: InterfaceMask) -> Result<InterfaceMask, OpenError> {
        let supported = InterfaceMask::ALL;
        let allowed_flags = supported | InterfaceMask::WRITABLE;
        let unsupported_bits = ifaces.bits() & !allowed_flags.bits();
        let requested = ifaces & supported;
        let old_opened = self.opened;
        let new_opened = old_opened | requested;

        if self.gone {
            return Err(OpenError {
                opened: self.opened,
                error: raw_error(ERROR_DEVICE_NOT_CONNECTED),
            });
        }
        if new_opened != old_opened {
            if let Err(error) = self.ensure_device() {
                return Err(OpenError {
                    opened: self.opened,
                    error,
                });
            }
            let old_plan = windows_protocol::report_modes(old_opened, self.extension_kind);
            let new_plan = windows_protocol::report_modes(new_opened, self.extension_kind);
            let plan_changed = old_plan != new_plan;
            if plan_changed {
                self.cancel_read();
            }
            self.opened = new_opened;
            let setup = (|| {
                self.start_read()?;
                if new_opened.contains(InterfaceMask::IR)
                    && (plan_changed || !old_opened.contains(InterfaceMask::IR))
                {
                    self.setup_ir_camera(true)?;
                }
                self.configure_report_mode(plan_changed)?;
                self.start_read()?;
                if !self.discovery_started {
                    self.request_status()?;
                }
                self.activate_motion_plus()
            })();
            if let Err(error) = setup {
                self.cancel_read();
                self.opened = old_opened;
                if old_opened.is_empty() && !self.watch_enabled {
                    if new_opened.contains(InterfaceMask::IR) {
                        let _ = self.setup_ir_camera(false);
                    }
                    self.device = None;
                    self.ir_initialized = false;
                    self.ir_camera_mode = None;
                    self.pending_acks = [None; ACK_QUEUE_CAPACITY];
                } else {
                    if old_opened.contains(InterfaceMask::IR) {
                        let _ = self.setup_ir_camera(true);
                    } else if new_opened.contains(InterfaceMask::IR) {
                        let _ = self.setup_ir_camera(false);
                    }
                    let _ = self.configure_report_mode(true);
                    let _ = self.start_read();
                }
                return Err(OpenError {
                    opened: self.opened,
                    error,
                });
            }
            if new_opened.contains(InterfaceMask::CORE) && !old_opened.contains(InterfaceMask::CORE)
            {
                self.buttons_seeded = false;
            }
            if let Some(interface) = self.extension_kind.interface()
                && !new_opened.contains(interface)
            {
                self.extension_buttons_seeded = false;
            }
        }

        if unsupported_bits != 0 {
            return Err(OpenError {
                opened: self.opened,
                error: unsupported("Windows HID received unknown interface-mask bits"),
            });
        }
        Ok(self.opened)
    }

    pub fn close(&mut self, ifaces: InterfaceMask) {
        let new_opened = self.opened & !(ifaces & InterfaceMask::ALL);
        if new_opened == self.opened {
            return;
        }
        let old_opened = self.opened;
        if old_opened.contains(InterfaceMask::CORE) && !new_opened.contains(InterfaceMask::CORE) {
            self.buttons_seeded = false;
        }
        if let Some(interface) = self.extension_kind.interface()
            && old_opened.contains(interface)
            && !new_opened.contains(interface)
        {
            self.extension_buttons_seeded = false;
        }
        if old_opened.contains(InterfaceMask::IR)
            && !new_opened.contains(InterfaceMask::IR)
            && self.setup_ir_camera(false).is_err()
        {
            self.mark_gone();
            return;
        }
        self.pending = [None; BUTTON_QUEUE_CAPACITY];
        self.pending_pos = 0;
        self.pending_len = 0;
        self.opened = new_opened;

        if new_opened.is_empty() {
            if self.rumble_on.replace(false) && self.device.is_some() {
                let _ = self.write_report_packet(&[0x10, 0]);
            }
            self.current_mode = None;
            self.report_plan = None;
            if self.watch_enabled {
                if self.start_read().is_err() || self.request_status().is_err() {
                    self.mark_gone();
                }
            } else {
                self.cancel_read();
                self.device = None;
                self.buttons = 0;
                self.buttons_seeded = false;
                self.extension_buttons = 0;
                self.extension_buttons_seeded = false;
                self.extension_kind = windows_protocol::ExtensionKind::None;
                self.extension_connected = None;
                self.motion_plus_present = false;
                self.motion_plus_initialized = false;
                self.motion_plus_active = false;
                self.motion_plus_mode = None;
                self.motion_plus_extension_connected = None;
                self.balance_calibration = None;
                self.memory_read = None;
                self.discovery_started = false;
                self.reinitialize_after_memory = false;
                self.pending_acks = [None; ACK_QUEUE_CAPACITY];
                self.battery.set(None);
                self.leds.set(None);
                self.update_available();
            }
            return;
        }

        let plan_changed = windows_protocol::report_modes(old_opened, self.extension_kind)
            != windows_protocol::report_modes(new_opened, self.extension_kind);
        if plan_changed {
            self.cancel_read();
            if self.start_read().is_err() {
                self.mark_gone();
                return;
            }
            if new_opened.contains(InterfaceMask::IR) && self.setup_ir_camera(true).is_err() {
                self.mark_gone();
                return;
            }
            if self.configure_report_mode(true).is_err() {
                self.mark_gone();
            }
        }
    }

    /// Watches status reports for extension connection changes.
    pub fn watch(&mut self, enabled: bool) -> io::Result<()> {
        if self.watch_enabled == enabled {
            return Ok(());
        }
        if enabled {
            if self.opened.is_empty() {
                self.ensure_device()?;
                if let Err(error) = self.start_read().and_then(|()| self.request_status()) {
                    self.cancel_read();
                    self.device = None;
                    return Err(error);
                }
            }
            self.watch_enabled = true;
        } else {
            self.watch_enabled = false;
            if self.opened.is_empty() {
                self.cancel_read();
                self.device = None;
            }
        }
        Ok(())
    }
    pub fn dispatch(&mut self) -> io::Result<Event> {
        if let Some(event) = self.dequeue() {
            return Ok(event);
        }
        if self.gone {
            if !self.gone_reported {
                self.gone_reported = true;
                return Ok(Event {
                    time: Timestamp::default(),
                    kind: EventKind::Gone,
                });
            }
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Wii Remote is gone",
            ));
        }
        if self.opened.is_empty() && !self.watch_enabled {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "no Wii Remote interfaces are open or watched",
            ));
        }

        loop {
            if !self.read_pending
                && let Err(error) = self.start_read()
            {
                if Self::disconnected_error(&error) {
                    self.mark_gone();
                    return self.dispatch();
                }
                return Err(error);
            }
            match unsafe { WaitForSingleObject(self.read_event.raw(), 0) } {
                WAIT_OBJECT_0 => {
                    let handle = self.device_handle()?;
                    let mut bytes = 0u32;
                    self.read_pending = false;
                    if unsafe { GetOverlappedResult(handle, &*self.read_overlapped, &mut bytes, 0) }
                        == 0
                    {
                        let error = last_error();
                        if Self::disconnected_error(&error) {
                            self.mark_gone();
                            return self.dispatch();
                        }
                        return Err(error);
                    }
                    if bytes == 0 {
                        self.mark_gone();
                        return self.dispatch();
                    }
                    if let Err(error) = self.decode_report(bytes as usize) {
                        if Self::disconnected_error(&error) {
                            self.mark_gone();
                            return self.dispatch();
                        }
                        return Err(error);
                    }
                    if let Some(event) = self.dequeue() {
                        return Ok(event);
                    }
                }
                WAIT_TIMEOUT => {
                    return Err(io::Error::from(io::ErrorKind::WouldBlock));
                }
                _ => return Err(last_error()),
            }
        }
    }

    pub fn rumble(&mut self, on: bool) -> io::Result<()> {
        if self.opened.is_empty() && !self.watch_enabled {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "no Wii Remote interfaces are open",
            ));
        }
        self.ensure_device()?;
        let old_rumble = self.rumble_on.get();
        self.rumble_on.set(on);
        let result = if let Some(mode) = self.current_mode {
            self.write_report_mode(mode)
        } else {
            self.write_report_packet(&[
                0x10,
                if on {
                    windows_protocol::REPORT_OUTPUT_COMMON_RUMBLE
                } else {
                    0
                },
            ])
        };
        if let Err(error) = result {
            self.rumble_on.set(old_rumble);
            return Err(error);
        }
        Ok(())
    }

    pub fn get_led(&self, led: usize) -> io::Result<bool> {
        if led >= 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "LED index must be in 0..4",
            ));
        }
        if self.device.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Wii Remote is not connected",
            ));
        }
        let mask = self
            .leds
            .get()
            .ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))?;
        Ok(mask & (1 << led) != 0)
    }

    pub fn set_led(&self, led: usize, on: bool) -> io::Result<()> {
        if led >= 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "LED index must be in 0..4",
            ));
        }
        if self.device.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Wii Remote is not connected",
            ));
        }
        let old_mask = self
            .leds
            .get()
            .ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))?;
        let led_bit = 1 << led;
        let new_mask = if on {
            old_mask | led_bit
        } else {
            old_mask & !led_bit
        };
        self.write_report_packet(&[
            windows_protocol::REPORT_LED_OUTPUT,
            (new_mask << 4) | u8::from(self.rumble_on.get()),
        ])?;
        self.leds.set(Some(new_mask));
        Ok(())
    }

    pub fn battery(&self) -> io::Result<u8> {
        if self.device.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Wii Remote is not connected",
            ));
        }
        self.battery
            .get()
            .ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))
    }

    pub fn attr(&self, _name: &str) -> io::Result<Vec<u8>> {
        Err(unsupported(
            "Linux sysfs attributes are not available through Windows HID",
        ))
    }

    pub fn set_mp_normalization(&mut self, x: i32, y: i32, z: i32, factor: i32) {
        self.motion_plus.set(x, y, z, factor);
        self.motion_plus_offset_remainder = Axis3::default();
    }

    pub fn mp_normalization(&self) -> ([i32; 3], i32) {
        let (x, y, z, factor) = self.motion_plus.values();
        ([x, y, z], factor)
    }
}

impl Drop for Interface {
    fn drop(&mut self) {
        self.cancel_read();
    }
}

fn create_event() -> io::Result<Handle> {
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        Err(last_error())
    } else {
        Ok(Handle(event))
    }
}

#[cfg(test)]
mod tests {
    use super::windows_protocol::{ExtensionKind, decode_data};
    use super::{NINTENDO_VENDOR_ID, VHF_TEST_PRODUCT_ID, VHF_TEST_VENDOR_ID, is_wiimote_identity};
    use crate::model::Axis3;

    #[test]
    fn physical_identity_filters_out_the_local_virtual_hid() {
        assert!(is_wiimote_identity(NINTENDO_VENDOR_ID, 0x0306));
        assert!(is_wiimote_identity(NINTENDO_VENDOR_ID, 0x0330));
        assert!(!is_wiimote_identity(
            VHF_TEST_VENDOR_ID,
            VHF_TEST_PRODUCT_ID
        ));
        assert!(!is_wiimote_identity(NINTENDO_VENDOR_ID, 0x0001));
    }

    #[test]
    fn report_30_keeps_sensor_bits_out_of_core_button_state() {
        let report = [0x30, 0x61, 0xe1];
        let buttons = decode_data(&report, ExtensionKind::None, false, None)
            .unwrap()
            .unwrap()
            .buttons
            .unwrap();
        assert_eq!(buttons & (1 << 0), 1 << 0);
        assert_eq!(buttons & (1 << 5), 1 << 5);
        assert_eq!(buttons & (1 << 10), 1 << 10);
        assert_eq!(buttons & (1 << 6), 0);
        assert_eq!(buttons & (1 << 9), 0);
    }

    #[test]
    fn report_31_reconstructs_centered_accelerometer_values() {
        let report = [0x31, 0x60, 0x60, 0x80, 0x80, 0x80];
        let frame = decode_data(&report, ExtensionKind::None, false, None)
            .unwrap()
            .unwrap();
        assert_eq!(frame.accel, Some(Axis3 { x: 3, y: 2, z: 2 }));
        assert!(decode_data(&report[..5], ExtensionKind::None, false, None).is_err());
    }
}
