use std::ffi::{OsStr, c_void};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, EqualSid, GetSecurityDescriptorDacl, GetTokenInformation,
    IsValidSid, LookupAccountNameW, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    PSID, RevertToSelf, SECURITY_ATTRIBUTES, SID_AND_ATTRIBUTES, TOKEN_GROUPS,
    TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER, TokenGroups, TokenLogonSid, TokenSessionId,
    TokenUser,
};
use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
use windows_sys::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_SESSIONSTATE_UNLOCK, WTSActive, WTSFreeMemory, WTSINFOEXW,
    WTSQuerySessionInformationW, WTSQueryUserToken, WTSSessionInfoEx,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
};

const SERVICE_ACCOUNT_NAME: &str = r"NT SERVICE\WiiLandOutput";

const LOGON_ID_GROUP_ATTRIBUTES: u32 = 0xC000_0000;
const ENABLED_GROUP_ATTRIBUTE: u32 = 0x0000_0004;
// Pipe data/read-attributes/synchronize access without FILE_CREATE_PIPE_INSTANCE.
pub(crate) const PIPE_CLIENT_ACCESS: u32 = 0x0010_0083;

pub(crate) struct OwnedHandle(pub(crate) HANDLE);
// Kernel handles are process-wide opaque values; concurrent use is governed by
// each Windows API and the service's ownership/synchronization rules.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct Sid {
    storage: Vec<usize>,
    offset: usize,
}

impl Sid {
    fn pointer(&self) -> PSID {
        unsafe {
            self.storage
                .as_ptr()
                .cast::<u8>()
                .add(self.offset)
                .cast_mut()
                .cast()
        }
    }
}

pub(crate) struct LogonIdentity {
    pub(crate) session_id: u32,
    user_sid: Sid,
    logon_sid: Sid,
}

impl LogonIdentity {
    pub(crate) fn logon_sid_string(&self) -> io::Result<String> {
        sid_to_string(self.logon_sid.pointer())
    }
}

pub(crate) struct PipeSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
    pub(crate) attributes: SECURITY_ATTRIBUTES,
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            unsafe { LocalFree(self.descriptor.cast()) };
        }
    }
}

pub(crate) fn verify_local_service_identity() -> io::Result<()> {
    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);
    let service_sid = lookup_service_sid()?;
    if !token_contains_enabled_sid(token.0, service_sid.pointer())? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "WiiLandOutput service SID is absent or disabled; install the service with `sc.exe sidtype WiiLandOutput unrestricted` and run it as LocalSystem",
        ));
    }
    let user_sid = token_sid(token.0, TokenUser)?;
    if !sid_matches_text(user_sid.pointer(), "S-1-5-18")? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "WiiLandOutput must run as LocalSystem because active-console token access requires it",
        ));
    }
    Ok(())
}

pub(crate) fn active_logon_identity(session_id: u32) -> io::Result<LogonIdentity> {
    let mut token = ptr::null_mut();
    if unsafe { WTSQueryUserToken(session_id, &mut token) } == 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!(
                "WTSQueryUserToken({session_id}) failed; WiiLandOutput must run as LocalSystem with an active console logon: {error}"
            ),
        ));
    }
    let token = OwnedHandle(token);
    let actual_session = token_session_id(token.0)?;
    if actual_session != session_id {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "active-console token session does not match the console session",
        ));
    }
    let user_sid = token_sid(token.0, TokenUser)?;
    let logon_sid = token_sid(token.0, TokenLogonSid)?;
    Ok(LogonIdentity {
        session_id,
        user_sid,
        logon_sid,
    })
}

pub(crate) fn session_is_unlocked(session_id: u32) -> io::Result<bool> {
    let mut information = ptr::null_mut();
    let mut bytes = 0u32;
    if unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session_id,
            WTSSessionInfoEx,
            &mut information,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if information.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WTSQuerySessionInformationW returned a null lock-state buffer",
        ));
    }
    let information = WtsBuffer(information.cast());
    if (bytes as usize) < size_of::<WTSINFOEXW>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WTS returned a truncated session lock-state structure",
        ));
    }
    let extended = unsafe { &*information.0.cast::<WTSINFOEXW>() };
    if extended.Level != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WTS returned an unsupported session information level",
        ));
    }
    let level = unsafe { extended.Data.WTSInfoExLevel1 };
    if level.SessionId != session_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WTS returned lock state for a different session",
        ));
    }
    if level.SessionState != WTSActive {
        return Ok(false);
    }
    Ok(level.SessionFlags as u32 == WTS_SESSIONSTATE_UNLOCK)
}

struct WtsBuffer(*mut c_void);

impl Drop for WtsBuffer {
    fn drop(&mut self) {
        unsafe { WTSFreeMemory(self.0) };
    }
}

pub(crate) fn system_pipe_security() -> io::Result<PipeSecurity> {
    pipe_security_from_sddl("D:P(A;;GA;;;SY)")
}

pub(crate) fn pipe_security(identity: &LogonIdentity) -> io::Result<PipeSecurity> {
    let logon_sid = identity.logon_sid_string()?;
    let sddl = format!(
        "D:P(A;;GA;;;SY)(A;;0x{:08X};;;{logon_sid})",
        PIPE_CLIENT_ACCESS
    );
    pipe_security_from_sddl(&sddl)
}

fn pipe_security_from_sddl(sddl: &str) -> io::Result<PipeSecurity> {
    let wide: Vec<u16> = OsStr::new(sddl).encode_wide().chain(Some(0)).collect();
    let mut descriptor = ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if descriptor.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a null named-pipe security descriptor",
        ));
    }
    Ok(PipeSecurity {
        descriptor,
        attributes: SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        },
    })
}

pub(crate) fn apply_pipe_security(pipe: HANDLE, security: &PipeSecurity) -> io::Result<()> {
    let mut dacl_present = 0;
    let mut dacl = ptr::null_mut();
    let mut dacl_defaulted = 0;
    if unsafe {
        GetSecurityDescriptorDacl(
            security.descriptor,
            &mut dacl_present,
            &mut dacl,
            &mut dacl_defaulted,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if dacl_present == 0 || dacl.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "named-pipe security descriptor has no explicit DACL",
        ));
    }
    let result = unsafe {
        SetSecurityInfo(
            pipe,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null_mut(),
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    Ok(())
}

pub(crate) fn verify_client(pipe: HANDLE, expected: &LogonIdentity) -> io::Result<()> {
    if unsafe { ImpersonateNamedPipeClient(pipe) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        let mut token = ptr::null_mut();
        if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle(token);
        if token_session_id(token.0)? != expected.session_id {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "named-pipe client token belongs to a different session",
            ));
        }
        let user_sid = token_sid(token.0, TokenUser)?;
        let logon_sid = token_sid(token.0, TokenLogonSid)?;
        if unsafe { EqualSid(user_sid.pointer(), expected.user_sid.pointer()) } == 0
            || unsafe { EqualSid(logon_sid.pointer(), expected.logon_sid.pointer()) } == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "named-pipe client token is not the active console logon",
            ));
        }
        Ok(())
    })();
    let reverted = unsafe { RevertToSelf() } != 0;
    if !reverted {
        return Err(io::Error::last_os_error());
    }
    result
}

pub(crate) fn verify_service_server(pipe: HANDLE) -> io::Result<()> {
    use windows_sys::Win32::System::Pipes::{
        GetNamedPipeServerProcessId, GetNamedPipeServerSessionId,
    };
    use windows_sys::Win32::System::Services::{
        OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_MANAGER_CONNECT,
        SC_STATUS_PROCESS_INFO, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS_PROCESS,
    };

    const SERVICE_NAME: &[u16] = &[
        87, 105, 105, 76, 97, 110, 100, 79, 117, 116, 112, 117, 116, 0,
    ];
    let mut process_id = 0u32;
    let mut session_id = u32::MAX;
    if unsafe { GetNamedPipeServerProcessId(pipe, &mut process_id) } == 0
        || unsafe { GetNamedPipeServerSessionId(pipe, &mut session_id) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if process_id == 0 || session_id != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "WiiLandOutput pipe server must be the session-0 service process",
        ));
    }

    let manager = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) };
    if manager.is_null() {
        return Err(io::Error::last_os_error());
    }
    let manager = ServiceHandle(manager);
    let service = unsafe { OpenServiceW(manager.0, SERVICE_NAME.as_ptr(), SERVICE_QUERY_STATUS) };
    if service.is_null() {
        return Err(io::Error::last_os_error());
    }
    let service = ServiceHandle(service);
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut returned = 0u32;
    if unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
            size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut returned,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if status.dwCurrentState != SERVICE_RUNNING || status.dwProcessId != process_id {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "WiiLandOutput pipe is not served by the running SCM-registered WiiLandOutput process",
        ));
    }
    Ok(())
}

struct ServiceHandle(windows_sys::Win32::System::Services::SC_HANDLE);

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Services::CloseServiceHandle(self.0);
        }
    }
}

fn token_session_id(token: HANDLE) -> io::Result<u32> {
    let mut session_id = 0u32;
    let mut returned = 0u32;
    if unsafe {
        GetTokenInformation(
            token,
            TokenSessionId,
            (&mut session_id as *mut u32).cast(),
            size_of::<u32>() as u32,
            &mut returned,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if returned as usize != size_of::<u32>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned an invalid token session ID",
        ));
    }
    Ok(session_id)
}

fn token_sid(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Sid> {
    let storage = token_information(token, class)?;
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
                    "token does not contain exactly one valid logon SID",
                ));
            }
            groups.Groups[0].Sid
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported Windows token SID class",
            ));
        }
    };
    sid_from_buffer(storage, sid)
}

fn token_contains_enabled_sid(token: HANDLE, wanted: PSID) -> io::Result<bool> {
    let storage = token_information(token, TokenGroups)?;
    let groups = unsafe { &*storage.as_ptr().cast::<TOKEN_GROUPS>() };
    let entries = ptr::addr_of!(groups.Groups).cast::<SID_AND_ATTRIBUTES>();
    for index in 0..groups.GroupCount as usize {
        let entry = unsafe { &*entries.add(index) };
        if entry.Attributes & ENABLED_GROUP_ATTRIBUTE != 0
            && unsafe { EqualSid(entry.Sid, wanted) } != 0
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn token_information(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<usize>> {
    let mut required = 0u32;
    let result = unsafe { GetTokenInformation(token, class, ptr::null_mut(), 0, &mut required) };
    if result != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned token data for a zero-sized query",
        ));
    }
    let error = unsafe { GetLastError() };
    if error != ERROR_INSUFFICIENT_BUFFER || required == 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let words = (required as usize).div_ceil(size_of::<usize>());
    let mut storage = vec![0usize; words];
    let capacity = words
        .checked_mul(size_of::<usize>())
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "token data is too large"))?;
    if unsafe {
        GetTokenInformation(
            token,
            class,
            storage.as_mut_ptr().cast(),
            capacity,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(storage)
}

fn sid_from_buffer(storage: Vec<usize>, sid: PSID) -> io::Result<Sid> {
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned an invalid token SID",
        ));
    }
    let base = storage.as_ptr() as usize;
    let address = sid as usize;
    let offset = address
        .checked_sub(base)
        .filter(|offset| *offset < storage.len() * size_of::<usize>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "token SID is out of bounds"))?;
    Ok(Sid { storage, offset })
}

fn lookup_service_sid() -> io::Result<Sid> {
    let account: Vec<u16> = OsStr::new(SERVICE_ACCOUNT_NAME)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut sid_bytes = 0u32;
    let mut domain_chars = 0u32;
    let mut use_type = 0;
    let first = unsafe {
        LookupAccountNameW(
            ptr::null(),
            account.as_ptr(),
            ptr::null_mut(),
            &mut sid_bytes,
            ptr::null_mut(),
            &mut domain_chars,
            &mut use_type,
        )
    };
    if first != 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER || sid_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Windows cannot resolve the `NT SERVICE\\WiiLandOutput` service SID; create the WiiLandOutput service before starting the broker",
        ));
    }
    let mut sid_storage = vec![0usize; (sid_bytes as usize).div_ceil(size_of::<usize>())];
    let mut domain = vec![0u16; domain_chars as usize];
    if unsafe {
        LookupAccountNameW(
            ptr::null(),
            account.as_ptr(),
            sid_storage.as_mut_ptr().cast(),
            &mut sid_bytes,
            domain.as_mut_ptr(),
            &mut domain_chars,
            &mut use_type,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let sid: PSID = sid_storage.as_mut_ptr().cast();
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned an invalid WiiLandOutput service SID",
        ));
    }
    Ok(Sid {
        storage: sid_storage,
        offset: 0,
    })
}

fn sid_matches_text(sid: PSID, text: &str) -> io::Result<bool> {
    let expected_wide: Vec<u16> = OsStr::new(text).encode_wide().chain(Some(0)).collect();
    let mut expected = ptr::null_mut();
    if unsafe {
        windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW(
            expected_wide.as_ptr(),
            &mut expected,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let equal = unsafe { EqualSid(sid, expected) } != 0;
    unsafe { LocalFree(expected.cast()) };
    Ok(equal)
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
    let mut length = 0usize;
    unsafe {
        while *wide.add(length) != 0 {
            length += 1;
        }
    }
    let text = unsafe { std::slice::from_raw_parts(wide, length) };
    let result = String::from_utf16(text).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned invalid SID text",
        )
    });
    unsafe { LocalFree(wide.cast()) };
    result
}
