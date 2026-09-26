use std::cell::Cell;
use std::ffi::c_void;
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::slice;
use std::time::{Duration, Instant};

use crate::windows_bootstrap::{
    BOOTSTRAP_ACCESS_MASK, BOOTSTRAP_DEADLINE, ClientAnnounce, ClientEcho, DAEMON_HELLO_LEN,
    DAEMON_READY_LEN, DaemonHello, DaemonReady, LogonSid, READY_STATUS_OK, ReturnPipeId,
    derive_return_pipe_name,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_INSUFFICIENT_BUFFER, ERROR_IO_PENDING, ERROR_PIPE_BUSY,
    ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GENERIC_ALL, GENERIC_READ, GENERIC_WRITE,
    GetLastError, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree, WAIT_FAILED, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACCESS_DENIED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid,
    GetAce, GetLengthSid, GetTokenInformation, IsValidSid, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, RevertToSelf, SECURITY_ATTRIBUTES, TOKEN_GROUPS,
    TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER, TokenLogonSid, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_CREATE_PIPE_INSTANCE, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    FILE_READ_DATA, FILE_WRITE_DATA, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WRITE_DAC, WRITE_OWNER, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, ImpersonateNamedPipeClient, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentThread, INFINITE, OpenProcessToken, OpenThreadToken,
    ResetEvent, WaitForSingleObject,
};

const PIPE_ENDPOINT_PREFIX: &str = r"\\.\pipe\WiiLand.";
const PIPE_ENDPOINT_SUFFIX: &str = ".daemon";
const MAX_FINITE_WAIT_MS: u32 = u32::MAX - 1;
const READ_CONTROL_ACCESS: u32 = 0x0002_0000;
const LOGON_ID_GROUP_ATTRIBUTES: u32 = 0xC000_0000;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
const INHERITED_ACE: u8 = 0x10;
const ERROR_SEM_TIMEOUT: u32 = 121;
const WRITE_DAC_AND_OWNER: u32 = WRITE_DAC | WRITE_OWNER;
// Rights rejected for non-logon ACEs, apart from the trusted machine principals.
const PIPE_DATA_OR_SECURITY_CHANGE_ACCESS: u32 = GENERIC_ALL
    | GENERIC_READ
    | GENERIC_WRITE
    | FILE_READ_DATA
    | FILE_WRITE_DATA
    | FILE_CREATE_PIPE_INSTANCE
    | WRITE_DAC
    | WRITE_OWNER;

/// The Windows named-pipe implementation behind the platform-neutral client.
pub(crate) struct WindowsPipe {
    handle: OwnedHandle,
    event: OwnedHandle,
    read_timeout: Cell<Option<Duration>>,
    write_timeout: Cell<Option<Duration>>,
}

impl WindowsPipe {
    pub(crate) fn connect(path: &Path) -> io::Result<Self> {
        let identity = CurrentIdentity::open()?;
        Self::connect_for_identity(path, &identity)
    }

    pub(crate) fn connect_default() -> io::Result<(PathBuf, Self)> {
        let identity = CurrentIdentity::open().map_err(|error| {
            io::Error::new(error.kind(), format!("querying current logon: {error}"))
        })?;
        let path = endpoint_path(&identity.logon_sid_name);
        let stream = Self::connect_for_identity(&path, &identity)?;
        Ok((path, stream))
    }

    /// Authenticates the daemon on a unique client-owned return pipe. The B-pipe
    /// descriptor check is an additional access-control check, not server identity.
    fn connect_for_identity(path: &Path, identity: &CurrentIdentity) -> io::Result<Self> {
        let expected = endpoint_path(&identity.logon_sid_name);
        if path != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows IPC must use the current logon-session pipe endpoint",
            ));
        }

        let deadline = Instant::now()
            .checked_add(BOOTSTRAP_DEADLINE)
            .ok_or_else(|| timeout_error("bootstrap deadline is out of range"))?;
        let pipe_name = nul_terminated_wide(path)?;
        let rendezvous = open_pipe(&pipe_name, deadline).map_err(|error| {
            io::Error::new(error.kind(), format!("opening daemon rendezvous: {error}"))
        })?;
        ensure_before_deadline(deadline, "bootstrap deadline exceeded")?;
        // The descriptor remains a useful ACL sanity check, but is not proof
        // of the server's identity; that is established by the R-pipe token.
        verify_pipe_security(rendezvous.0, identity).map_err(|error| {
            io::Error::new(error.kind(), format!("verifying daemon pipe DACL: {error}"))
        })?;
        ensure_before_deadline(deadline, "bootstrap deadline exceeded")?;

        let identifier = random_return_pipe_id(deadline)?;
        let logon_sid = LogonSid::new(&identity.logon_sid_name).map_err(bootstrap_error)?;
        let return_name = derive_return_pipe_name(&logon_sid, &identifier);
        let return_name = nul_terminated_wide(Path::new(&return_name))?;
        let return_pipe = create_return_pipe(&return_name, identity).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("creating client return pipe: {error}"),
            )
        })?;
        let event = create_event()?;
        ensure_before_deadline(deadline, "bootstrap deadline exceeded")?;

        let announce = ClientAnnounce::new(identifier).encode();
        write_all_until(rendezvous.0, event.0, &announce, deadline)?;
        connect_return_pipe(return_pipe.0, &event, deadline).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("connecting client return pipe: {error}"),
            )
        })?;

        let mut hello_record = [0_u8; DAEMON_HELLO_LEN];
        read_exact_until(return_pipe.0, event.0, &mut hello_record, deadline)?;
        let hello = DaemonHello::parse(&hello_record).map_err(bootstrap_error)?;
        authenticate_pipe_writer(return_pipe.0, identity, deadline).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("authenticating daemon hello: {error}"),
            )
        })?;

        let echo = ClientEcho::new(*hello.challenge()).encode();
        write_all_until(rendezvous.0, event.0, &echo, deadline)?;

        let mut ready_record = [0_u8; DAEMON_READY_LEN];
        read_exact_until(return_pipe.0, event.0, &mut ready_record, deadline)?;
        let ready = DaemonReady::parse(&ready_record).map_err(bootstrap_error)?;
        authenticate_pipe_writer(return_pipe.0, identity, deadline).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("authenticating daemon ready: {error}"),
            )
        })?;
        if ready.status() != READY_STATUS_OK {
            return Err(permission_denied("daemon rejected the IPC bootstrap"));
        }
        ensure_before_deadline(deadline, "bootstrap deadline exceeded")?;

        Ok(Self {
            handle: return_pipe,
            event,
            read_timeout: Cell::new(None),
            write_timeout: Cell::new(None),
        })
    }

    pub(crate) fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        validate_timeout(timeout)?;
        self.read_timeout.set(timeout);
        Ok(())
    }

    pub(crate) fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(self.read_timeout.get())
    }

    pub(crate) fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        validate_timeout(timeout)?;
        self.write_timeout.set(timeout);
        Ok(())
    }

    fn finish_transfer(
        &self,
        overlapped: &OVERLAPPED,
        timeout: Option<Duration>,
    ) -> io::Result<usize> {
        if let Err(error) = wait_for_event(self.event.0, timeout) {
            // Cancel and drain before either the OVERLAPPED record or caller
            // buffer can go out of scope. Preserve bytes if completion raced
            // with cancellation so framing never loses a successful transfer.
            unsafe {
                let _ = CancelIoEx(self.handle.0, overlapped);
                let mut transferred = 0;
                if GetOverlappedResult(self.handle.0, overlapped, &mut transferred, 1) != 0 {
                    return Ok(transferred as usize);
                }
            }
            return Err(error);
        }

        self.completed_transfer(overlapped)
    }

    fn completed_transfer(&self, overlapped: &OVERLAPPED) -> io::Result<usize> {
        let mut transferred = 0;
        let completed =
            unsafe { GetOverlappedResult(self.handle.0, overlapped, &mut transferred, 0) };
        if completed == 0 {
            return Err(last_pipe_error());
        }
        Ok(transferred as usize)
    }
}

impl Read for WindowsPipe {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }

        reset_event(&self.event)?;
        let mut overlapped = OVERLAPPED {
            hEvent: self.event.0,
            ..OVERLAPPED::default()
        };
        let count = buffer.len().min(u32::MAX as usize) as u32;
        let started = unsafe {
            ReadFile(
                self.handle.0,
                buffer.as_mut_ptr(),
                count,
                ptr::null_mut(),
                &mut overlapped,
            )
        };
        if started != 0 {
            return self.completed_transfer(&overlapped);
        }

        let error = unsafe { GetLastError() };
        if error != ERROR_IO_PENDING {
            return Err(pipe_error(error));
        }
        self.finish_transfer(&overlapped, self.read_timeout.get())
    }
}

impl Write for WindowsPipe {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }

        reset_event(&self.event)?;
        let mut overlapped = OVERLAPPED {
            hEvent: self.event.0,
            ..OVERLAPPED::default()
        };
        let count = buffer.len().min(u32::MAX as usize) as u32;
        let started = unsafe {
            WriteFile(
                self.handle.0,
                buffer.as_ptr(),
                count,
                ptr::null_mut(),
                &mut overlapped,
            )
        };
        if started != 0 {
            return self.completed_transfer(&overlapped);
        }

        let error = unsafe { GetLastError() };
        if error != ERROR_IO_PENDING {
            return Err(pipe_error(error));
        }
        self.finish_transfer(&overlapped, self.write_timeout.get())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Windows kernel handles are process-wide and may be moved between threads;
/// the client still serializes all pipe I/O through `&mut self`.
struct OwnedHandle(HANDLE);

unsafe impl Send for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = LocalFree(self.0 as HLOCAL);
            }
        }
    }
}

struct TokenSid {
    // `sid_offset` points into this owned, aligned token-information buffer.
    storage: Vec<usize>,
    sid_offset: usize,
}

impl TokenSid {
    fn pointer(&self) -> PSID {
        unsafe {
            self.storage
                .as_ptr()
                .cast::<u8>()
                .add(self.sid_offset)
                .cast_mut()
                .cast()
        }
    }
}

struct CurrentIdentity {
    user_sid: TokenSid,
    logon_sid: TokenSid,
    logon_sid_name: String,
}

impl CurrentIdentity {
    fn open() -> io::Result<Self> {
        let mut token = ptr::null_mut();
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if opened == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle(token);

        let user_sid = token_sid(token.0, TokenUser)?;
        let logon_sid = token_sid(token.0, TokenLogonSid)?;
        let logon_sid_name = sid_to_string(logon_sid.pointer())?;
        if !is_logon_sid_name(&logon_sid_name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process token does not contain a Windows logon SID",
            ));
        }

        Ok(Self {
            user_sid,
            logon_sid,
            logon_sid_name,
        })
    }
}

fn is_logon_sid_name(name: &str) -> bool {
    let mut components = name.split('-');
    components.next() == Some("S")
        && components.next() == Some("1")
        && components.next() == Some("5")
        && components.next() == Some("5")
        && components
            .next()
            .is_some_and(|value| value.parse::<u32>().is_ok())
        && components
            .next()
            .is_some_and(|value| value.parse::<u32>().is_ok())
        && components.next().is_none()
}

/// Return the current logon-session rendezvous endpoint. Its security descriptor
/// is checked as a supplementary ACL safeguard; the daemon's identity is checked
/// from the writer token on the unique return pipe.
pub(crate) fn default_pipe_path() -> io::Result<PathBuf> {
    let identity = CurrentIdentity::open()?;
    Ok(endpoint_path(&identity.logon_sid_name))
}

fn endpoint_path(logon_sid: &str) -> PathBuf {
    PathBuf::from(format!(
        "{PIPE_ENDPOINT_PREFIX}{logon_sid}{PIPE_ENDPOINT_SUFFIX}"
    ))
}

fn token_sid(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<TokenSid> {
    let mut required = 0;
    let queried = unsafe { GetTokenInformation(token, class, ptr::null_mut(), 0, &mut required) };
    if queried != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned token data for a zero-sized query",
        ));
    }
    let error = unsafe { GetLastError() };
    if error != ERROR_INSUFFICIENT_BUFFER || required == 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }

    let word_count = (required as usize).div_ceil(size_of::<usize>());
    let mut storage = vec![0usize; word_count];
    let buffer_bytes = word_count
        .checked_mul(size_of::<usize>())
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "token data is too large"))?;
    let queried = unsafe {
        GetTokenInformation(
            token,
            class,
            storage.as_mut_ptr().cast(),
            buffer_bytes,
            &mut required,
        )
    };
    if queried == 0 {
        return Err(io::Error::last_os_error());
    }

    let sid = unsafe {
        if class == TokenUser {
            (*storage.as_ptr().cast::<TOKEN_USER>()).User.Sid
        } else if class == TokenLogonSid {
            let groups = &*storage.as_ptr().cast::<TOKEN_GROUPS>();
            if groups.GroupCount != 1
                || groups.Groups[0].Attributes & LOGON_ID_GROUP_ATTRIBUTES
                    != LOGON_ID_GROUP_ATTRIBUTES
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "process token did not return exactly one logon SID",
                ));
            }
            groups.Groups[0].Sid
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported token SID information class",
            ));
        }
    };
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process token returned an invalid SID",
        ));
    }

    let base = storage.as_ptr() as usize;
    let sid_address = sid as usize;
    let sid_offset = sid_address
        .checked_sub(base)
        .filter(|offset| *offset < storage.len() * size_of::<usize>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "token SID is out of bounds"))?;
    Ok(TokenSid {
        storage,
        sid_offset,
    })
}

fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut wide = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut wide) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if wide.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a null SID string",
        ));
    }
    let _allocation = LocalAllocation(wide.cast());

    let mut length = 0;
    unsafe {
        while *wide.add(length) != 0 {
            length += 1;
        }
    }
    let text = unsafe { slice::from_raw_parts(wide, length) };
    String::from_utf16(text).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned invalid SID text",
        )
    })
}

fn open_pipe(pipe_name: &[u16], deadline: Instant) -> io::Result<OwnedHandle> {
    loop {
        ensure_before_deadline(deadline, "named-pipe connect deadline exceeded")?;

        // Omitting SECURITY_CONTEXT_TRACKING selects static SQOS tracking.
        let handle = unsafe {
            CreateFileW(
                pipe_name.as_ptr(),
                BOOTSTRAP_ACCESS_MASK,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                ptr::null_mut(),
            )
        };
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            return Ok(OwnedHandle(handle));
        }

        let error = unsafe { GetLastError() };
        if error != ERROR_PIPE_BUSY {
            return Err(pipe_error(error));
        }

        let milliseconds = duration_to_millis(deadline.saturating_duration_since(Instant::now()));
        if milliseconds == 0 {
            return Err(timeout_error("named-pipe connect deadline exceeded"));
        }
        let available = unsafe { WaitNamedPipeW(pipe_name.as_ptr(), milliseconds) };
        if available != 0 {
            continue;
        }
        let error = unsafe { GetLastError() };
        if error == ERROR_SEM_TIMEOUT && Instant::now() < deadline {
            continue;
        }
        if error == ERROR_SEM_TIMEOUT {
            return Err(timeout_error("named-pipe connect deadline exceeded"));
        }
        return Err(pipe_error(error));
    }
}

fn random_return_pipe_id(deadline: Instant) -> io::Result<ReturnPipeId> {
    loop {
        ensure_before_deadline(deadline, "bootstrap deadline exceeded")?;
        let mut bytes = [0_u8; 16];
        let status = unsafe {
            BCryptGenRandom(
                ptr::null_mut(),
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status < 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        ensure_before_deadline(deadline, "bootstrap deadline exceeded")?;
        if bytes.iter().all(|byte| *byte == 0) {
            continue;
        }
        return ReturnPipeId::from_bytes(bytes).map_err(bootstrap_error);
    }
}

fn create_return_pipe(pipe_name: &[u16], identity: &CurrentIdentity) -> io::Result<OwnedHandle> {
    let sddl = format!(
        "D:P(D;;0x{WRITE_DAC_AND_OWNER:08X};;;OW)(A;;0x{BOOTSTRAP_ACCESS_MASK:08X};;;{})",
        identity.logon_sid_name
    );
    let sddl_wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_wide.as_ptr(),
            1,
            &mut descriptor,
            ptr::null_mut(),
        )
    };
    let _descriptor = LocalAllocation(descriptor.cast());
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }
    if descriptor.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a null pipe security descriptor",
        ));
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let handle = unsafe {
        CreateNamedPipeW(
            pipe_name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            4096,
            4096,
            0,
            &attributes,
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(last_pipe_error())
    } else {
        Ok(OwnedHandle(handle))
    }
}

fn connect_return_pipe(handle: HANDLE, event: &OwnedHandle, deadline: Instant) -> io::Result<()> {
    ensure_before_deadline(deadline, "bootstrap deadline exceeded")?;
    reset_event(event)?;
    let mut overlapped = OVERLAPPED {
        hEvent: event.0,
        ..OVERLAPPED::default()
    };
    if unsafe { ConnectNamedPipe(handle, &mut overlapped) } != 0 {
        return ensure_before_deadline(deadline, "bootstrap deadline exceeded");
    }
    let error = unsafe { GetLastError() };
    if error == ERROR_PIPE_CONNECTED {
        return ensure_before_deadline(deadline, "bootstrap deadline exceeded");
    }
    if error != ERROR_IO_PENDING {
        return Err(pipe_error(error));
    }
    finish_bootstrap_transfer(handle, event.0, &overlapped, deadline)?;
    Ok(())
}

fn authenticate_pipe_writer(
    handle: HANDLE,
    identity: &CurrentIdentity,
    deadline: Instant,
) -> io::Result<()> {
    ensure_before_deadline(deadline, "bootstrap deadline exceeded")?;
    if unsafe { ImpersonateNamedPipeClient(handle) } == 0 {
        return Err(last_pipe_error());
    }
    let revert = RevertToSelfGuard;
    let result = (|| {
        let mut token = ptr::null_mut();
        if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle(token);
        let user_sid = token_sid(token.0, TokenUser)?;
        let logon_sid = token_sid(token.0, TokenLogonSid)?;
        if unsafe { EqualSid(user_sid.pointer(), identity.user_sid.pointer()) } == 0
            || unsafe { EqualSid(logon_sid.pointer(), identity.logon_sid.pointer()) } == 0
        {
            return Err(permission_denied(
                "named-pipe writer does not match the current user and logon session",
            ));
        }
        Ok(())
    })();
    drop(revert);
    result?;
    ensure_before_deadline(deadline, "bootstrap deadline exceeded")
}

struct RevertToSelfGuard;

impl Drop for RevertToSelfGuard {
    fn drop(&mut self) {
        if unsafe { RevertToSelf() } == 0 {
            std::process::abort();
        }
    }
}

fn read_exact_until(
    handle: HANDLE,
    event: HANDLE,
    mut buffer: &mut [u8],
    deadline: Instant,
) -> io::Result<()> {
    while !buffer.is_empty() {
        ensure_before_deadline(deadline, "bootstrap deadline exceeded")?;
        reset_event_handle(event)?;
        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..OVERLAPPED::default()
        };
        let count = buffer.len().min(u32::MAX as usize) as u32;
        let started = unsafe {
            ReadFile(
                handle,
                buffer.as_mut_ptr(),
                count,
                ptr::null_mut(),
                &mut overlapped,
            )
        };
        let transferred = if started != 0 {
            finish_synchronous_bootstrap_transfer(handle, &overlapped, deadline)?
        } else {
            let error = unsafe { GetLastError() };
            if error != ERROR_IO_PENDING {
                return Err(pipe_error(error));
            }
            finish_bootstrap_transfer(handle, event, &overlapped, deadline)?
        };
        if transferred == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "named-pipe peer closed during bootstrap",
            ));
        }
        let (_, remaining) = buffer.split_at_mut(transferred.min(buffer.len()));
        buffer = remaining;
    }
    Ok(())
}

fn write_all_until(
    handle: HANDLE,
    event: HANDLE,
    mut buffer: &[u8],
    deadline: Instant,
) -> io::Result<()> {
    while !buffer.is_empty() {
        ensure_before_deadline(deadline, "bootstrap deadline exceeded")?;
        reset_event_handle(event)?;
        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..OVERLAPPED::default()
        };
        let count = buffer.len().min(u32::MAX as usize) as u32;
        let started = unsafe {
            WriteFile(
                handle,
                buffer.as_ptr(),
                count,
                ptr::null_mut(),
                &mut overlapped,
            )
        };
        let transferred = if started != 0 {
            finish_synchronous_bootstrap_transfer(handle, &overlapped, deadline)?
        } else {
            let error = unsafe { GetLastError() };
            if error != ERROR_IO_PENDING {
                return Err(pipe_error(error));
            }
            finish_bootstrap_transfer(handle, event, &overlapped, deadline)?
        };
        if transferred == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "named-pipe write made no progress during bootstrap",
            ));
        }
        buffer = &buffer[transferred.min(buffer.len())..];
    }
    Ok(())
}

fn finish_synchronous_bootstrap_transfer(
    handle: HANDLE,
    overlapped: &OVERLAPPED,
    deadline: Instant,
) -> io::Result<usize> {
    let transferred = completed_transfer_handle(handle, overlapped)?;
    ensure_before_deadline(deadline, "bootstrap deadline exceeded")?;
    Ok(transferred)
}

fn finish_bootstrap_transfer(
    handle: HANDLE,
    event: HANDLE,
    overlapped: &OVERLAPPED,
    deadline: Instant,
) -> io::Result<usize> {
    if let Err(error) = wait_for_event_until(event, deadline) {
        unsafe {
            let _ = CancelIoEx(handle, overlapped);
            let mut transferred = 0;
            let _ = GetOverlappedResult(handle, overlapped, &mut transferred, 1);
        }
        return Err(error);
    }
    let transferred = completed_transfer_handle(handle, overlapped)?;
    ensure_before_deadline(deadline, "bootstrap deadline exceeded")?;
    Ok(transferred)
}

fn completed_transfer_handle(handle: HANDLE, overlapped: &OVERLAPPED) -> io::Result<usize> {
    let mut transferred = 0;
    if unsafe { GetOverlappedResult(handle, overlapped, &mut transferred, 0) } == 0 {
        Err(last_pipe_error())
    } else {
        Ok(transferred as usize)
    }
}

fn ensure_before_deadline(deadline: Instant, message: &'static str) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(timeout_error(message))
    } else {
        Ok(())
    }
}

fn wait_for_event_until(event: HANDLE, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timeout_error("bootstrap deadline exceeded"));
        }
        match unsafe { WaitForSingleObject(event, duration_to_millis(remaining)) } {
            WAIT_OBJECT_0 => {
                return ensure_before_deadline(deadline, "bootstrap deadline exceeded");
            }
            WAIT_TIMEOUT if Instant::now() < deadline => continue,
            WAIT_TIMEOUT => return Err(timeout_error("bootstrap deadline exceeded")),
            WAIT_FAILED => return Err(io::Error::last_os_error()),
            _ => {
                return Err(io::Error::other(
                    "WaitForSingleObject returned an unexpected status",
                ));
            }
        }
    }
}

fn verify_pipe_security(handle: HANDLE, identity: &CurrentIdentity) -> io::Result<()> {
    // Check the rendezvous endpoint's owner and DACL as an ACL safeguard only.
    // Matching metadata is not server authentication; the return-pipe writer
    // token is checked separately after each daemon bootstrap record.
    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let result = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    let _descriptor = LocalAllocation(descriptor.cast());
    if result != 0 {
        // Do not connect when the server's ownership and DACL cannot be checked.
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    if owner.is_null()
        || dacl.is_null()
        || unsafe { EqualSid(owner, identity.user_sid.pointer()) } == 0
    {
        return Err(permission_denied(
            "named-pipe owner is not the current user or its DACL is absent",
        ));
    }

    let acl_start = dacl as usize;
    let acl_size = usize::from(unsafe { (*dacl).AclSize });
    let Some(acl_header_end) = acl_start.checked_add(size_of::<ACL>()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "named-pipe DACL has an invalid size",
        ));
    };
    let Some(acl_end) = acl_start.checked_add(acl_size) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "named-pipe DACL has an invalid size",
        ));
    };
    if acl_size < size_of::<ACL>() || acl_header_end > acl_end {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "named-pipe DACL is truncated",
        ));
    }

    let ace_count = unsafe { (*dacl).AceCount };
    let mut logon_sid_read = false;
    let mut logon_sid_write = false;
    let mut logon_sid_read_control = false;
    for index in 0..u32::from(ace_count) {
        let mut ace = ptr::null_mut();
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 || ace.is_null() {
            return Err(io::Error::last_os_error());
        }
        let ace_start = ace as usize;
        let Some(ace_header_end) = ace_start.checked_add(size_of::<ACE_HEADER>()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "named-pipe DACL contains an out-of-bounds ACE",
            ));
        };
        if ace_start < acl_header_end || ace_header_end > acl_end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "named-pipe DACL contains an out-of-bounds ACE",
            ));
        }
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        let ace_size = usize::from(header.AceSize);
        if ace_size < size_of::<ACE_HEADER>()
            || ace_start
                .checked_add(ace_size)
                .is_none_or(|ace_end| ace_end > acl_end)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "named-pipe DACL contains an out-of-bounds ACE",
            ));
        }

        match header.AceType {
            ACCESS_ALLOWED_ACE_TYPE => {
                if ace_size < size_of::<ACCESS_ALLOWED_ACE>() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "named-pipe DACL contains a truncated allow ACE",
                    ));
                }
                let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
                let sid = ptr::addr_of!(allowed.SidStart).cast_mut().cast();
                let sid = checked_ace_sid(sid, ace_size)?;
                let sid_name = sid_to_string(sid)?;
                if is_broad_principal(&sid_name) {
                    return Err(permission_denied(
                        "named-pipe DACL grants access to a broad Windows principal",
                    ));
                }
                let is_logon_sid = unsafe { EqualSid(sid, identity.logon_sid.pointer()) } != 0;
                if allowed.Mask & PIPE_DATA_OR_SECURITY_CHANGE_ACCESS != 0
                    && !is_logon_sid
                    && !is_trusted_pipe_principal(&sid_name)
                {
                    return Err(permission_denied(
                        "named-pipe DACL grants data or security changes to an untrusted principal",
                    ));
                }
                if is_logon_sid && header.AceFlags & INHERITED_ACE == 0 {
                    let grants_all = allowed.Mask & GENERIC_ALL != 0;
                    logon_sid_read |=
                        grants_all || allowed.Mask & (GENERIC_READ | FILE_READ_DATA) != 0;
                    logon_sid_write |=
                        grants_all || allowed.Mask & (GENERIC_WRITE | FILE_WRITE_DATA) != 0;
                    logon_sid_read_control |= grants_all
                        || allowed.Mask & (GENERIC_READ | GENERIC_WRITE | READ_CONTROL_ACCESS) != 0;
                }
            }
            ACCESS_DENIED_ACE_TYPE => {
                if ace_size < size_of::<ACCESS_DENIED_ACE>() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "named-pipe DACL contains a truncated deny ACE",
                    ));
                }
                let denied = unsafe { &*ace.cast::<ACCESS_DENIED_ACE>() };
                let sid = ptr::addr_of!(denied.SidStart).cast_mut().cast();
                let sid = checked_ace_sid(sid, ace_size)?;
                if unsafe {
                    EqualSid(sid, identity.logon_sid.pointer()) != 0
                        || EqualSid(sid, identity.user_sid.pointer()) != 0
                } {
                    return Err(permission_denied(
                        "named-pipe DACL denies the current user or logon SID",
                    ));
                }
            }
            _ => {
                return Err(permission_denied(
                    "named-pipe DACL contains an ACE type the client cannot verify",
                ));
            }
        }
    }
    if !logon_sid_read || !logon_sid_write || !logon_sid_read_control {
        return Err(permission_denied(
            "named-pipe DACL does not grant read/write and READ_CONTROL to this logon SID",
        ));
    }
    Ok(())
}

fn checked_ace_sid(sid: PSID, ace_size: usize) -> io::Result<PSID> {
    let sid_offset = size_of::<ACE_HEADER>() + size_of::<u32>();
    const SID_HEADER_SIZE: usize = 8;
    let minimum_ace_size = sid_offset + SID_HEADER_SIZE;
    if ace_size < minimum_ace_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "named-pipe DACL contains a truncated ACE SID",
        ));
    }

    let subauthority_count = unsafe { *sid.cast::<u8>().add(1) } as usize;
    let Some(sid_size) = subauthority_count
        .checked_mul(size_of::<u32>())
        .and_then(|subauthority_bytes| SID_HEADER_SIZE.checked_add(subauthority_bytes))
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "named-pipe DACL contains an out-of-bounds ACE SID",
        ));
    };
    if sid_offset.checked_add(sid_size) != Some(ace_size) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "named-pipe DACL contains an out-of-bounds ACE SID",
        ));
    }
    if unsafe { IsValidSid(sid) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "named-pipe DACL contains an invalid ACE SID",
        ));
    }
    if unsafe { GetLengthSid(sid) } as usize != sid_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "named-pipe DACL contains an invalid ACE SID length",
        ));
    }
    Ok(sid)
}

fn is_trusted_pipe_principal(sid: &str) -> bool {
    // LocalSystem and the built-in Administrators group are trusted machine-wide principals.
    matches!(sid, "S-1-5-18" | "S-1-5-32-544")
}

fn is_broad_principal(sid: &str) -> bool {
    matches!(
        sid,
        "S-1-1-0" // Everyone
            | "S-1-5-7" // Anonymous
            | "S-1-5-11" // Authenticated Users
            | "S-1-5-4" // Interactive
            | "S-1-5-13" // Terminal Server Users
            | "S-1-5-14" // Remote Interactive Logon
            | "S-1-5-32-545" // Builtin Users
            | "S-1-5-32-546" // Builtin Guests
            | "S-1-5-2" // Network
            | "S-1-15-2-1" // All Application Packages
    )
}

fn nul_terminated_wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "named-pipe path contains a NUL character",
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn create_event() -> io::Result<OwnedHandle> {
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(OwnedHandle(event))
    }
}

fn reset_event(event: &OwnedHandle) -> io::Result<()> {
    reset_event_handle(event.0)
}

fn reset_event_handle(event: HANDLE) -> io::Result<()> {
    if unsafe { ResetEvent(event) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn wait_for_event(event: HANDLE, timeout: Option<Duration>) -> io::Result<()> {
    if timeout.is_some_and(|duration| duration.is_zero()) {
        return Err(timeout_error("named-pipe I/O timed out"));
    }
    let deadline = timeout.and_then(|duration| Instant::now().checked_add(duration));
    loop {
        let milliseconds = match (timeout, deadline) {
            (None, _) => INFINITE,
            (Some(_), Some(deadline)) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(timeout_error("named-pipe I/O timed out"));
                }
                duration_to_millis(remaining)
            }
            (Some(_), None) => MAX_FINITE_WAIT_MS,
        };
        let result = unsafe { WaitForSingleObject(event, milliseconds) };
        match result {
            WAIT_OBJECT_0 => return Ok(()),
            WAIT_TIMEOUT if deadline.is_none_or(|deadline| Instant::now() < deadline) => continue,
            WAIT_TIMEOUT => return Err(timeout_error("named-pipe I/O timed out")),
            WAIT_FAILED => return Err(io::Error::last_os_error()),
            _ => {
                return Err(io::Error::other(
                    "WaitForSingleObject returned an unexpected status",
                ));
            }
        }
    }
}

fn duration_to_millis(duration: Duration) -> u32 {
    if duration.is_zero() {
        return 0;
    }
    let rounded = duration.as_millis().saturating_add(u128::from(
        !duration.subsec_nanos().is_multiple_of(1_000_000),
    ));
    rounded.min(u128::from(MAX_FINITE_WAIT_MS)) as u32
}

fn validate_timeout(timeout: Option<Duration>) -> io::Result<()> {
    if timeout.is_some_and(|duration| duration.is_zero()) {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "timeout must be non-zero",
        ))
    } else {
        Ok(())
    }
}

fn timeout_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, message)
}

fn bootstrap_error(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn permission_denied(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn last_pipe_error() -> io::Error {
    pipe_error(unsafe { GetLastError() })
}

fn pipe_error(code: u32) -> io::Error {
    match code {
        ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED => {
            io::Error::new(io::ErrorKind::UnexpectedEof, "named-pipe peer disconnected")
        }
        _ => io::Error::from_raw_os_error(code as i32),
    }
}
