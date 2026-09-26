use crate::protocol::{PIPE_NAME, REQUEST_SIZE, RESPONSE_SIZE};
use crate::windows::security::{self, OwnedHandle, PipeSecurity};
use std::ffi::OsStr;
use std::io;
use std::mem::zeroed;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GetLastError,
    HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, ReadFile, WRITE_DAC,
    WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, WaitForMultipleObjects, WaitForSingleObject,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const PIPE_INSTANCE_COUNT: usize = 32;

pub(crate) struct Pipe(OwnedHandle);

impl Pipe {
    pub(crate) fn create(security: &PipeSecurity, first_instance: bool) -> io::Result<Self> {
        let name: Vec<u16> = OsStr::new(PIPE_NAME).encode_wide().chain(Some(0)).collect();
        let first_instance_flag = if first_instance {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | WRITE_DAC | FILE_FLAG_OVERLAPPED | first_instance_flag,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_INSTANCE_COUNT as u32,
                RESPONSE_SIZE as u32,
                REQUEST_SIZE as u32,
                0,
                &security.attributes,
            )
        };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            let error = unsafe { GetLastError() };
            return Err(io::Error::new(
                io::Error::from_raw_os_error(error as i32).kind(),
                format!(
                    "could not create protected WiiLandOutput pipe; `WiiLandOutput` must own its first pipe instance: {}",
                    io::Error::from_raw_os_error(error as i32)
                ),
            ));
        }
        Ok(Self(OwnedHandle(handle)))
    }

    pub(crate) fn set_security(&self, security: &PipeSecurity) -> io::Result<()> {
        security::apply_pipe_security(self.raw(), security)
    }

    pub(crate) fn disconnect(&self) -> io::Result<()> {
        if unsafe { DisconnectNamedPipe(self.raw()) } != 0 {
            return Ok(());
        }
        let error = unsafe { GetLastError() };
        if error == ERROR_PIPE_NOT_CONNECTED || error == ERROR_NO_DATA {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(error as i32))
        }
    }
    pub(crate) fn raw(&self) -> HANDLE {
        self.0.0
    }

    pub(crate) fn connect(
        &self,
        event: &OverlappedEvent,
        state_change: HANDLE,
    ) -> io::Result<bool> {
        event.reset()?;
        let mut overlapped = event.overlapped();
        if unsafe { ConnectNamedPipe(self.raw(), &mut overlapped) } != 0 {
            return Ok(true);
        }
        let error = unsafe { GetLastError() };
        if error == ERROR_PIPE_CONNECTED {
            return Ok(true);
        }
        if error != ERROR_IO_PENDING {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        if !self.wait_io(event, state_change, &mut overlapped, None)? {
            return Ok(false);
        }
        let mut transferred = 0u32;
        if unsafe { GetOverlappedResult(self.raw(), &overlapped, &mut transferred, 0) } == 0 {
            let error = unsafe { GetLastError() };
            if error == ERROR_PIPE_CONNECTED || is_disconnect_error(error) {
                return Ok(true);
            }
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        Ok(true)
    }

    pub(crate) fn read_request(
        &self,
        event: &OverlappedEvent,
        state_change: HANDLE,
        report_deadline: Option<Instant>,
    ) -> io::Result<[u8; REQUEST_SIZE]> {
        let mut bytes = [0; REQUEST_SIZE];
        self.read_exact(
            &mut bytes,
            event,
            state_change,
            io_deadline(REQUEST_TIMEOUT, report_deadline),
        )?;
        Ok(bytes)
    }

    pub(crate) fn write_response(
        &self,
        bytes: &[u8; RESPONSE_SIZE],
        event: &OverlappedEvent,
        state_change: HANDLE,
        report_deadline: Option<Instant>,
    ) -> io::Result<()> {
        self.write_all(
            bytes,
            event,
            state_change,
            io_deadline(RESPONSE_TIMEOUT, report_deadline),
        )
    }

    fn read_exact(
        &self,
        buffer: &mut [u8],
        event: &OverlappedEvent,
        state_change: HANDLE,
        deadline: Instant,
    ) -> io::Result<()> {
        let mut offset = 0usize;
        while offset < buffer.len() {
            if Instant::now() >= deadline {
                return Err(timed_out());
            }
            event.reset()?;
            let mut overlapped = event.overlapped();
            let mut transferred = 0u32;
            let result = unsafe {
                ReadFile(
                    self.raw(),
                    buffer[offset..].as_mut_ptr(),
                    (buffer.len() - offset) as u32,
                    &mut transferred,
                    &mut overlapped,
                )
            };
            if result == 0 {
                let error = unsafe { GetLastError() };
                if error != ERROR_IO_PENDING {
                    if is_disconnect_error(error) {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "output client disconnected",
                        ));
                    }
                    return Err(io::Error::from_raw_os_error(error as i32));
                }
                if !self.wait_io(event, state_change, &mut overlapped, Some(deadline))? {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "service gate changed",
                    ));
                }
                if unsafe { GetOverlappedResult(self.raw(), &overlapped, &mut transferred, 0) } == 0
                {
                    let error = unsafe { GetLastError() };
                    if is_disconnect_error(error) {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "output client disconnected",
                        ));
                    }
                    return Err(io::Error::from_raw_os_error(error as i32));
                }
            }
            if Instant::now() >= deadline {
                return Err(timed_out());
            }
            if transferred == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "output client closed its pipe",
                ));
            }
            offset += transferred as usize;
        }
        Ok(())
    }

    fn write_all(
        &self,
        buffer: &[u8],
        event: &OverlappedEvent,
        state_change: HANDLE,
        deadline: Instant,
    ) -> io::Result<()> {
        let mut offset = 0usize;
        while offset < buffer.len() {
            if Instant::now() >= deadline {
                return Err(timed_out());
            }
            event.reset()?;
            let mut overlapped = event.overlapped();
            let mut transferred = 0u32;
            let result = unsafe {
                WriteFile(
                    self.raw(),
                    buffer[offset..].as_ptr(),
                    (buffer.len() - offset) as u32,
                    &mut transferred,
                    &mut overlapped,
                )
            };
            if result == 0 {
                let error = unsafe { GetLastError() };
                if error != ERROR_IO_PENDING {
                    if is_disconnect_error(error) {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "output client disconnected",
                        ));
                    }
                    return Err(io::Error::from_raw_os_error(error as i32));
                }
                if !self.wait_io(event, state_change, &mut overlapped, Some(deadline))? {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "service gate changed",
                    ));
                }
                if unsafe { GetOverlappedResult(self.raw(), &overlapped, &mut transferred, 0) } == 0
                {
                    let error = unsafe { GetLastError() };
                    if is_disconnect_error(error) {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "output client disconnected",
                        ));
                    }
                    return Err(io::Error::from_raw_os_error(error as i32));
                }
            }
            if Instant::now() >= deadline {
                return Err(timed_out());
            }
            if transferred == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "zero-length output pipe write",
                ));
            }
            offset += transferred as usize;
        }
        Ok(())
    }

    fn wait_io(
        &self,
        event: &OverlappedEvent,
        state_change: HANDLE,
        overlapped: &mut OVERLAPPED,
        deadline: Option<Instant>,
    ) -> io::Result<bool> {
        let handles = [event.raw(), state_change];
        let timeout = deadline.map_or(INFINITE, remaining_milliseconds);
        let result =
            unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, timeout) };
        if result == WAIT_OBJECT_0 {
            return Ok(true);
        }
        if result == WAIT_OBJECT_0 + 1 {
            self.cancel_and_drain(event, overlapped);
            return Ok(false);
        }
        if result == WAIT_TIMEOUT {
            self.cancel_and_drain(event, overlapped);
            return Err(timed_out());
        }
        let error = io::Error::last_os_error();
        self.cancel_and_drain(event, overlapped);
        Err(error)
    }

    fn cancel_and_drain(&self, event: &OverlappedEvent, overlapped: &mut OVERLAPPED) {
        unsafe { CancelIoEx(self.raw(), overlapped) };
        unsafe { WaitForSingleObject(event.raw(), INFINITE) };
        let mut ignored = 0u32;
        unsafe { GetOverlappedResult(self.raw(), overlapped, &mut ignored, 0) };
    }
}

pub(crate) struct OverlappedEvent(OwnedHandle);

impl OverlappedEvent {
    pub(crate) fn new() -> io::Result<Self> {
        let handle = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(OwnedHandle(handle)))
    }

    fn raw(&self) -> HANDLE {
        self.0.0
    }

    fn reset(&self) -> io::Result<()> {
        if unsafe { ResetEvent(self.raw()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn overlapped(&self) -> OVERLAPPED {
        let mut overlapped: OVERLAPPED = unsafe { zeroed() };
        overlapped.hEvent = self.raw();
        overlapped
    }
}

fn io_deadline(timeout: Duration, report_deadline: Option<Instant>) -> Instant {
    let request_deadline = Instant::now() + timeout;
    report_deadline.map_or(request_deadline, |deadline| deadline.min(request_deadline))
}

fn is_disconnect_error(error: u32) -> bool {
    error == windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE
        || error == windows_sys::Win32::Foundation::ERROR_PIPE_NOT_CONNECTED
        || error == windows_sys::Win32::Foundation::ERROR_NO_DATA
}

fn remaining_milliseconds(deadline: Instant) -> u32 {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let milliseconds = remaining.as_nanos().div_ceil(1_000_000);
    milliseconds.min(u32::MAX as u128) as u32
}

fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "WiiLandOutput pipe I/O exceeded its request or report deadline",
    )
}
