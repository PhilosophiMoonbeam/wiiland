use crate::protocol::{
    MAX_SLOTS, Operation, OutputLease, OutputReport, PIPE_NAME, REPORT_LAYOUT_VERSION,
    RESPONSE_SIZE, Request, Response,
};
use crate::windows::security::OwnedHandle;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::mem::zeroed;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::ptr;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    ERROR_IO_PENDING, ERROR_PIPE_BUSY, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, ReadFile, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, WaitForSingleObject,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(2);
const ERROR_SEM_TIMEOUT: u32 = 121;

/// A verified connection to the local WiiLandOutput v3 service.
///
/// The service creates the virtual HID collections, not this client. Reports
/// are serialized on this connection, and leases are private to the connection.
pub struct OutputClient {
    pipe: Option<File>,
    event: OwnedHandle,
    leases: [Option<OutputLease>; MAX_SLOTS],
}

impl OutputClient {
    /// Connect to the pipe and verify it belongs to the running SCM-registered
    /// `WiiLandOutput` service process in session 0.
    pub fn connect() -> io::Result<Self> {
        let pipe = open_pipe()?;
        crate::windows::security::verify_service_server(pipe.as_raw_handle())?;
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut client = Self {
            pipe: Some(pipe),
            event: OwnedHandle(event),
            leases: [None; MAX_SLOTS],
        };
        client.heartbeat()?;
        Ok(client)
    }

    /// Confirm that the connected service is speaking the v3 broker protocol.
    ///
    /// Heartbeats confirm transport liveness only. A client with an active
    /// lease must keep publishing full Gamepad and SupplementalAxes reports;
    /// each report ID expires two seconds after its last successful submission
    /// (or lease creation). Scheduling cannot guarantee a 500 ms interval if
    /// the broker stalls; the service revokes leases at the deadline.
    pub fn heartbeat(&mut self) -> io::Result<()> {
        let response = self.exchange(Request::heartbeat())?;
        self.check_status(response.status)?;
        if response.operation != Operation::Heartbeat
            || response.slot != 0
            || response.generation != 0
            || response.report_layout_version != REPORT_LAYOUT_VERSION
        {
            self.close_connection();
            return Err(invalid_data(
                "invalid HEARTBEAT response from WiiLandOutput",
            ));
        }
        Ok(())
    }

    /// Reserve one of the 32 fixed VHID slots for this pipe connection.
    pub fn create(&mut self) -> io::Result<OutputLease> {
        let response = self.exchange(Request::create())?;
        self.check_status(response.status)?;
        if response.operation != Operation::Create
            || response.slot as usize >= MAX_SLOTS
            || response.generation == 0
            || response.report_layout_version != REPORT_LAYOUT_VERSION
            || self.leases[response.slot as usize].is_some()
        {
            self.close_connection();
            return Err(invalid_data("invalid CREATE result from WiiLandOutput"));
        }
        let lease = OutputLease {
            slot: response.slot,
            generation: response.generation,
        };
        self.leases[response.slot as usize] = Some(lease);
        Ok(lease)
    }

    /// Submit a full report to a live lease. Payload shape is validated when
    /// the `OutputReport` is constructed, and the kernel driver validates ranges.
    /// Only a successful report refreshes that report ID's two-second deadline;
    /// heartbeat and reports with another ID do not.
    pub fn submit(&mut self, lease: OutputLease, report: &OutputReport) -> io::Result<()> {
        self.require_lease(lease)?;
        let response = self.exchange(Request::report(lease, report))?;
        self.check_response(response, Operation::Report, lease)
    }

    /// Neutralize and destroy a lease. Disconnecting without calling this also
    /// releases it: the service tears down all leases owned by a closed pipe.
    pub fn destroy(&mut self, lease: OutputLease) -> io::Result<()> {
        self.require_lease(lease)?;
        let response = self.exchange(Request::destroy(lease))?;
        self.check_response(response, Operation::Destroy, lease)?;
        self.leases[lease.slot as usize] = None;
        Ok(())
    }

    fn require_lease(&self, lease: OutputLease) -> io::Result<()> {
        if self.pipe.is_none() {
            return Err(not_connected());
        }
        if self.leases.get(lease.slot as usize).copied().flatten() == Some(lease) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "lease is not active on this WiiLandOutput connection",
            ))
        }
    }

    fn exchange(&mut self, request: Request) -> io::Result<Response> {
        let result = self.exchange_inner(request);
        if result.is_err() {
            self.close_connection();
        }
        result
    }

    fn exchange_inner(&mut self, request: Request) -> io::Result<Response> {
        let pipe = self.pipe.as_ref().ok_or_else(not_connected)?;
        let handle = pipe.as_raw_handle() as HANDLE;
        let deadline = Instant::now() + EXCHANGE_TIMEOUT;
        let bytes = request.encode();
        write_all(handle, self.event.0, &bytes, deadline)?;
        let mut response_bytes = [0; RESPONSE_SIZE];
        read_exact(handle, self.event.0, &mut response_bytes, deadline)?;
        let response = Response::decode(&response_bytes)?;
        if response.operation != request.operation {
            return Err(invalid_data("WiiLandOutput response operation mismatch"));
        }
        Ok(response)
    }

    fn close_connection(&mut self) {
        drop(self.pipe.take());
        self.leases = [None; MAX_SLOTS];
    }

    fn check_status(&self, status: u32) -> io::Result<()> {
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    }

    fn check_response(
        &mut self,
        response: Response,
        operation: Operation,
        lease: OutputLease,
    ) -> io::Result<()> {
        self.check_status(response.status)?;
        if response.operation != operation
            || response.slot != lease.slot
            || response.generation != lease.generation
            || response.report_layout_version != REPORT_LAYOUT_VERSION
        {
            self.close_connection();
            return Err(invalid_data("invalid WiiLandOutput lease response"));
        }
        Ok(())
    }
}

fn write_all(handle: HANDLE, event: HANDLE, buffer: &[u8], deadline: Instant) -> io::Result<()> {
    let mut offset = 0usize;
    while offset < buffer.len() {
        if Instant::now() >= deadline {
            return Err(exchange_timed_out());
        }
        let transferred = overlapped_transfer(
            handle,
            event,
            buffer[offset..].as_ptr().cast_mut(),
            (buffer.len() - offset) as u32,
            true,
            deadline,
        )?;
        if transferred == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "zero-length output pipe write",
            ));
        }
        offset += transferred;
    }
    Ok(())
}

fn read_exact(
    handle: HANDLE,
    event: HANDLE,
    buffer: &mut [u8],
    deadline: Instant,
) -> io::Result<()> {
    let mut offset = 0usize;
    while offset < buffer.len() {
        if Instant::now() >= deadline {
            return Err(exchange_timed_out());
        }
        let transferred = overlapped_transfer(
            handle,
            event,
            buffer[offset..].as_mut_ptr(),
            (buffer.len() - offset) as u32,
            false,
            deadline,
        )?;
        if transferred == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "output service closed its pipe",
            ));
        }
        offset += transferred;
    }
    Ok(())
}

fn overlapped_transfer(
    pipe: HANDLE,
    event: HANDLE,
    buffer: *mut u8,
    length: u32,
    write: bool,
    deadline: Instant,
) -> io::Result<usize> {
    if unsafe { ResetEvent(event) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    overlapped.hEvent = event;
    let mut transferred = 0u32;
    let result = unsafe {
        if write {
            WriteFile(
                pipe,
                buffer.cast_const(),
                length,
                &mut transferred,
                &mut overlapped,
            )
        } else {
            ReadFile(pipe, buffer, length, &mut transferred, &mut overlapped)
        }
    };
    if result != 0 {
        if Instant::now() >= deadline {
            return Err(exchange_timed_out());
        }
        return Ok(transferred as usize);
    }
    let error = unsafe { GetLastError() };
    if error != ERROR_IO_PENDING {
        return Err(io::Error::from_raw_os_error(error as i32));
    }

    let waited = unsafe {
        WaitForSingleObject(
            event,
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(u32::MAX as u128) as u32,
        )
    };
    if waited == WAIT_OBJECT_0 {
        if unsafe { GetOverlappedResult(pipe, &overlapped, &mut transferred, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if Instant::now() >= deadline {
            return Err(exchange_timed_out());
        }
        return Ok(transferred as usize);
    }
    let error = if waited == WAIT_TIMEOUT {
        exchange_timed_out()
    } else {
        io::Error::last_os_error()
    };
    cancel_and_drain(pipe, event, &mut overlapped);
    Err(error)
}

fn cancel_and_drain(pipe: HANDLE, event: HANDLE, overlapped: &mut OVERLAPPED) {
    unsafe { CancelIoEx(pipe, overlapped) };
    unsafe { WaitForSingleObject(event, INFINITE) };
    let mut ignored = 0u32;
    unsafe { GetOverlappedResult(pipe, overlapped, &mut ignored, 0) };
}

fn open_pipe() -> io::Result<File> {
    let name: Vec<u16> = OsStr::new(PIPE_NAME).encode_wide().chain(Some(0)).collect();
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                crate::windows::security::PIPE_CLIENT_ACCESS,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                ptr::null_mut(),
            )
        };
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            let file = unsafe { File::from_raw_handle(handle) };
            return Ok(file);
        }

        let error = unsafe { GetLastError() };
        if error != ERROR_PIPE_BUSY {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_ms = remaining.as_millis().min(u32::MAX as u128) as u32;
        if timeout_ms == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "WiiLandOutput pipe remained busy until the connect deadline",
            ));
        }
        if unsafe { WaitNamedPipeW(name.as_ptr(), timeout_ms) } == 0 {
            let wait_error = unsafe { GetLastError() };
            if wait_error == ERROR_SEM_TIMEOUT && Instant::now() < deadline {
                continue;
            }
            return Err(io::Error::from_raw_os_error(wait_error as i32));
        }
    }
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn not_connected() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "WiiLandOutput connection is closed",
    )
}

fn exchange_timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "WiiLandOutput request/response exceeded its 2-second deadline",
    )
}
