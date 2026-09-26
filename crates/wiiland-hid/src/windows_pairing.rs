//! Windows Bluetooth Classic discovery and Wii Remote pairing.
//!
//! Pairing is scoped to the selected local radio and remote Bluetooth address.
//! The helper never removes a device or changes unrelated pairings.

use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use windows_sys::Win32::Devices::Bluetooth::{
    BLUETOOTH_AUTHENTICATE_RESPONSE, BLUETOOTH_AUTHENTICATION_CALLBACK_PARAMS,
    BLUETOOTH_AUTHENTICATION_METHOD_LEGACY, BLUETOOTH_AUTHENTICATION_METHOD_NUMERIC_COMPARISON,
    BLUETOOTH_AUTHENTICATION_METHOD_PASSKEY_NOTIFICATION, BLUETOOTH_DEVICE_INFO,
    BLUETOOTH_DEVICE_SEARCH_PARAMS, BLUETOOTH_FIND_RADIO_PARAMS,
    BLUETOOTH_MITM_ProtectionNotRequired, BLUETOOTH_NUMERIC_COMPARISON_INFO,
    BLUETOOTH_PASSKEY_INFO, BLUETOOTH_PIN_INFO, BLUETOOTH_RADIO_INFO, BLUETOOTH_SERVICE_ENABLE,
    BluetoothAuthenticateDeviceEx, BluetoothFindDeviceClose, BluetoothFindFirstDevice,
    BluetoothFindFirstRadio, BluetoothFindNextDevice, BluetoothFindNextRadio,
    BluetoothFindRadioClose, BluetoothGetDeviceInfo, BluetoothGetRadioInfo,
    BluetoothRegisterForAuthenticationEx, BluetoothSendAuthenticationResponseEx,
    BluetoothSetServiceState, BluetoothUnregisterAuthentication, HBLUETOOTH_DEVICE_FIND,
    HBLUETOOTH_RADIO_FIND,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_CANCELLED, ERROR_NO_MORE_ITEMS, ERROR_NOT_FOUND, GetLastError, HANDLE,
    SetLastError,
};
use windows_sys::core::GUID;

const MAX_INQUIRY_TIMEOUT_MULTIPLIER: u8 = 48;
const WIIMOTE_HID_SERVICE: GUID = GUID::from_u128(0x00001124_0000_1000_8000_00805f9b34fb);
// BluetoothSetServiceState documents E_INVALIDARG when the requested service
// state is already active. The API returns this HRESULT in its DWORD result.
const HRESULT_INVALID_ARGUMENT: u32 = 0x8007_0057;
// Windows invokes only one authentication registration for a process while an
// authentication exchange is active. Keep this helper's exchanges serialized.
static PAIRING_LOCK: Mutex<()> = Mutex::new(());

/// A Bluetooth Classic address, stored in the conventional display order.
///
/// For example, `00:1E:35:3B:7E:6D` is represented by
/// `[0x00, 0x1e, 0x35, 0x3b, 0x7e, 0x6d]`. Wii Remote PIN bytes use the
/// reverse (least-significant-byte first) order required by its protocol.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BluetoothAddress([u8; 6]);

impl BluetoothAddress {
    /// Creates an address from its six bytes in conventional display order.
    pub const fn from_bytes(bytes: [u8; 6]) -> Self {
        Self(bytes)
    }

    /// Returns the six address bytes in conventional display order.
    pub const fn as_bytes(&self) -> &[u8; 6] {
        &self.0
    }

    fn pin_bytes(self) -> [u8; 6] {
        [
            self.0[5], self.0[4], self.0[3], self.0[2], self.0[1], self.0[0],
        ]
    }

    fn to_windows(self) -> windows_sys::Win32::Devices::Bluetooth::BLUETOOTH_ADDRESS {
        let value = self
            .0
            .iter()
            .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte));
        let mut address = windows_sys::Win32::Devices::Bluetooth::BLUETOOTH_ADDRESS::default();
        address.Anonymous.ullLong = value;
        address
    }

    fn from_windows(address: windows_sys::Win32::Devices::Bluetooth::BLUETOOTH_ADDRESS) -> Self {
        let value = unsafe { address.Anonymous.ullLong } & 0x0000_ffff_ffff_ffff;
        Self([
            (value >> 40) as u8,
            (value >> 32) as u8,
            (value >> 24) as u8,
            (value >> 16) as u8,
            (value >> 8) as u8,
            value as u8,
        ])
    }
}

impl fmt::Display for BluetoothAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if index != 0 {
                formatter.write_str(":")?;
            }
            write!(formatter, "{byte:02X}")?;
        }
        Ok(())
    }
}

/// A local Bluetooth Classic radio available to Windows.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Radio {
    /// Stable local adapter identity used to scope discovery and pairing.
    pub address: BluetoothAddress,
    /// Windows-reported adapter name.
    pub name: String,
}

/// A remote Bluetooth device reported by the selected local radio.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Device {
    /// Stable remote identity. Pass this address to [`pair_device`].
    pub address: BluetoothAddress,
    /// Windows-reported device name; names are not unique identities.
    pub name: String,
    /// Windows class-of-device value.
    pub class_of_device: u32,
    /// Whether Windows currently reports an active connection.
    pub connected: bool,
    /// Whether Windows remembers the device.
    pub remembered: bool,
    /// Whether Windows already has an authenticated link key for the device.
    pub authenticated: bool,
}

/// Which Wii Remote action was used to make it discoverable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PairingMethod {
    /// The red SYNC button uses the local radio address as the six-byte PIN.
    RedSyncButton,
    /// Pressing 1+2 uses the selected remote address as the six-byte PIN.
    OneAndTwoButtons,
}

/// Thread-safe cooperative cancellation for [`pair_device`].
///
/// Clone this value and call [`cancel`](Self::cancel) from another thread while
/// pairing is in progress. Windows exposes no cancellation handle for its
/// synchronous authentication call; cancellation is delivered as a negative
/// authentication response when Windows raises the registered callback. A call
/// still waiting for a device/authentication request can therefore run until
/// Windows or the remote device times it out.
#[derive(Clone, Debug, Default)]
pub struct PairingCancellation {
    cancelled: Arc<AtomicBool>,
}

impl PairingCancellation {
    /// Creates a fresh, not-cancelled pairing token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. The request is observed by the authentication callback.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Result of authenticating and enabling the Wii Remote HID service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PairingResult {
    /// The selected remote address that was paired.
    pub device: BluetoothAddress,
    /// Whether the device was already authenticated before this call.
    pub was_already_authenticated: bool,
    /// The Windows HID service is enabled (or was already enabled).
    pub hid_service_enabled: bool,
}

/// Failure while enumerating, authenticating, or enabling the Wii Remote HID service.
#[derive(Debug)]
#[non_exhaustive]
pub enum PairingError {
    /// No local Bluetooth radio is present or the selected adapter disappeared.
    NoBluetoothRadio,
    /// The enumerated radio is no longer available.
    RadioNotFound(BluetoothAddress),
    /// The device could not be found on the selected radio.
    DeviceNotFound(BluetoothAddress),
    /// Bluetooth inquiry timeout multiplier must be between 1 and 48.
    InvalidInquiryTimeout(u8),
    /// Pairing was cancelled before or during authentication.
    Cancelled,
    /// The remote requested a pairing method other than legacy PIN authentication.
    UnsupportedAuthenticationMethod(i32),
    /// A Windows Bluetooth API failed with the given status code.
    Windows {
        /// Name of the failed Windows operation.
        operation: &'static str,
        /// Native Win32 error or HRESULT returned by the operation.
        code: u32,
    },
    /// Authentication succeeded, but Windows could not enable the HID service.
    HidServiceEnable {
        /// Native Win32 error or HRESULT returned by the operation.
        code: u32,
    },
}

impl fmt::Display for PairingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoBluetoothRadio => formatter.write_str(
                "Windows found no Bluetooth radio; enable or connect a Bluetooth adapter and retry",
            ),
            Self::RadioNotFound(address) => write!(
                formatter,
                "Bluetooth radio {address} is no longer available; rescan local radios and select an available adapter",
            ),
            Self::DeviceNotFound(address) => write!(
                formatter,
                "Bluetooth device {address} is no longer available on the selected radio; wake the Wii Remote, put it in discovery mode, and rescan",
            ),
            Self::InvalidInquiryTimeout(value) => write!(
                formatter,
                "Bluetooth inquiry timeout multiplier {value} is invalid; use a value from 1 through 48",
            ),
            Self::Cancelled => formatter.write_str("Bluetooth pairing was cancelled"),
            Self::UnsupportedAuthenticationMethod(method) => write!(
                formatter,
                "Wii Remote requested unsupported Bluetooth authentication method {method}; use legacy PIN pairing by pressing red SYNC or 1+2, then retry",
            ),
            Self::Windows {
                operation: "BluetoothAuthenticateDeviceEx",
                code,
            } => write!(
                formatter,
                "BluetoothAuthenticateDeviceEx failed with Windows status {code} ({}); wake the Wii Remote, put it into discovery mode, and retry with the matching red SYNC or 1+2 pairing method",
                io::Error::from_raw_os_error(*code as i32),
            ),
            Self::Windows {
                operation: "BluetoothSendAuthenticationResponseEx",
                code: ERROR_CANCELLED,
            } => formatter.write_str(
                "BluetoothSendAuthenticationResponseEx was rejected; verify that the Wii Remote is awake and that you used the matching red SYNC or 1+2 pairing method",
            ),
            Self::Windows { operation, code } => write!(
                formatter,
                "{operation} failed with Windows status {code} ({}); check that Bluetooth is enabled and the selected Wii Remote is awake and discoverable",
                io::Error::from_raw_os_error(*code as i32),
            ),
            Self::HidServiceEnable { code } => write!(
                formatter,
                "the Wii Remote paired successfully, but Windows could not enable its HID service (status {code}: {}); verify the Windows Bluetooth/HID drivers and reconnect the remote",
                io::Error::from_raw_os_error(*code as i32),
            ),
        }
    }
}

impl Error for PairingError {}

/// Enumerates local Bluetooth Classic radios.
///
/// An empty result means Windows currently exposes no Bluetooth radios.
pub fn enumerate_radios() -> Result<Vec<Radio>, PairingError> {
    let mut radios = Vec::new();
    let mut radio_handle: HANDLE = std::ptr::null_mut();
    let parameters = BLUETOOTH_FIND_RADIO_PARAMS {
        dwSize: std::mem::size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32,
    };

    unsafe { SetLastError(0) };
    let find_handle = unsafe { BluetoothFindFirstRadio(&parameters, &mut radio_handle) };
    if find_handle.is_null() {
        let code = unsafe { GetLastError() };
        if code == 0 || code == ERROR_NO_MORE_ITEMS {
            return Ok(radios);
        }
        return Err(PairingError::Windows {
            operation: "BluetoothFindFirstRadio",
            code,
        });
    }
    let _find_handle = RadioFindHandle(find_handle);
    if radio_handle.is_null() {
        return Ok(radios);
    }

    loop {
        let radio = RadioHandle(radio_handle);
        radios.push(read_radio_info(radio.0)?);

        radio_handle = std::ptr::null_mut();
        unsafe { SetLastError(0) };
        if unsafe { BluetoothFindNextRadio(find_handle, &mut radio_handle) } == 0 {
            let code = unsafe { GetLastError() };
            if code != 0 && code != ERROR_NO_MORE_ITEMS {
                return Err(PairingError::Windows {
                    operation: "BluetoothFindNextRadio",
                    code,
                });
            }
            break;
        }
        if radio_handle.is_null() {
            break;
        }
    }

    Ok(radios)
}

/// Finds nearby, remembered, authenticated, and connected devices on one radio.
///
/// `timeout_multiplier` is the Windows inquiry multiplier in the range 1..=48;
/// each multiplier represents approximately 1.28 seconds of inquiry time.
pub fn enumerate_devices(
    radio: BluetoothAddress,
    timeout_multiplier: u8,
) -> Result<Vec<Device>, PairingError> {
    if !(1..=MAX_INQUIRY_TIMEOUT_MULTIPLIER).contains(&timeout_multiplier) {
        return Err(PairingError::InvalidInquiryTimeout(timeout_multiplier));
    }

    let radio_handle = open_radio(radio)?;
    let search = BLUETOOTH_DEVICE_SEARCH_PARAMS {
        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32,
        fReturnAuthenticated: 1,
        fReturnRemembered: 1,
        fReturnUnknown: 1,
        fReturnConnected: 1,
        fIssueInquiry: 1,
        cTimeoutMultiplier: timeout_multiplier,
        hRadio: radio_handle.0,
    };
    let mut info = empty_device_info();

    unsafe { SetLastError(0) };
    let find_handle = unsafe { BluetoothFindFirstDevice(&search, &mut info) };
    if find_handle.is_null() {
        let code = unsafe { GetLastError() };
        if code == 0 || code == ERROR_NO_MORE_ITEMS {
            return Ok(Vec::new());
        }
        return Err(PairingError::Windows {
            operation: "BluetoothFindFirstDevice",
            code,
        });
    }

    let _find_handle = DeviceFindHandle(find_handle);
    let mut devices = Vec::new();
    loop {
        devices.push(device_from_info(&info));
        info = empty_device_info();
        unsafe { SetLastError(0) };
        if unsafe { BluetoothFindNextDevice(find_handle, &mut info) } == 0 {
            let code = unsafe { GetLastError() };
            if code != 0 && code != ERROR_NO_MORE_ITEMS {
                return Err(PairingError::Windows {
                    operation: "BluetoothFindNextDevice",
                    code,
                });
            }
            break;
        }
    }

    Ok(devices)
}

/// Pairs the selected Wii Remote and enables its Windows HID service.
///
/// A red SYNC-button pairing uses the selected radio's Bluetooth address as a
/// six-byte PIN. The 1+2-button flow uses the remote's selected address. Both
/// PINs are raw reversed address bytes with an explicit length of six, retaining
/// any embedded zero byte; they are never treated as strings.
///
/// Only `radio` and `device` are touched. This operation never unpairs or
/// deletes other devices. Clone `cancellation` before calling this function to
/// request cancellation from another thread.
///
/// Calls to this helper are serialized because Windows permits only one
/// application authentication callback per active pairing exchange.
pub fn pair_device(
    radio: BluetoothAddress,
    device: BluetoothAddress,
    method: PairingMethod,
    cancellation: &PairingCancellation,
) -> Result<PairingResult, PairingError> {
    if cancellation.is_cancelled() {
        return Err(PairingError::Cancelled);
    }
    let _pairing_guard = PAIRING_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cancellation.is_cancelled() {
        return Err(PairingError::Cancelled);
    }

    let radio_handle = open_radio(radio)?;
    let mut device_info = empty_device_info();
    device_info.Address = device.to_windows();
    let status = unsafe { BluetoothGetDeviceInfo(radio_handle.0, &mut device_info) };
    if status != 0 {
        if status == ERROR_NOT_FOUND {
            return Err(PairingError::DeviceNotFound(device));
        }
        return Err(PairingError::Windows {
            operation: "BluetoothGetDeviceInfo",
            code: status,
        });
    }

    let mut was_already_authenticated = device_info.fAuthenticated != 0;
    if !was_already_authenticated {
        let pin = match method {
            PairingMethod::RedSyncButton => radio.pin_bytes(),
            PairingMethod::OneAndTwoButtons => device.pin_bytes(),
        };
        let callback_context = AuthenticationContext {
            radio: radio_handle.0,
            device,
            pin,
            cancelled: Arc::clone(&cancellation.cancelled),
            response_error: AtomicU32::new(0),
            unsupported_method: AtomicI32::new(0),
            callback_progress: Mutex::new(CallbackProgress::default()),
            callback_finished: Condvar::new(),
        };
        let mut registration_handle = 0_isize;
        let status = unsafe {
            BluetoothRegisterForAuthenticationEx(
                &device_info,
                &mut registration_handle,
                Some(authentication_callback),
                (&callback_context as *const AuthenticationContext).cast::<c_void>(),
            )
        };
        if status != 0 {
            return Err(PairingError::Windows {
                operation: "BluetoothRegisterForAuthenticationEx",
                code: status,
            });
        }
        let registration = AuthenticationRegistration(registration_handle);

        if cancellation.is_cancelled() {
            drop(registration);
            return Err(PairingError::Cancelled);
        }
        let authentication_status = unsafe {
            BluetoothAuthenticateDeviceEx(
                std::ptr::null_mut(),
                radio_handle.0,
                &mut device_info,
                std::ptr::null(),
                BLUETOOTH_MITM_ProtectionNotRequired,
            )
        };
        if authentication_status == 0 {
            callback_context.wait_for_callback();
        } else {
            callback_context.wait_for_idle_callbacks();
        }
        drop(registration);

        if authentication_status == ERROR_NO_MORE_ITEMS {
            was_already_authenticated = true;
        }
        if authentication_status != 0
            && authentication_status != ERROR_NO_MORE_ITEMS
            && cancellation.is_cancelled()
        {
            return Err(PairingError::Cancelled);
        }
        let unsupported_method = callback_context.unsupported_method.load(Ordering::Acquire);
        if unsupported_method != 0 {
            return Err(PairingError::UnsupportedAuthenticationMethod(
                unsupported_method,
            ));
        }
        let response_error = callback_context.response_error.load(Ordering::Acquire);
        if response_error != 0 {
            return Err(PairingError::Windows {
                operation: "BluetoothSendAuthenticationResponseEx",
                code: response_error,
            });
        }
        if authentication_status != 0 && authentication_status != ERROR_NO_MORE_ITEMS {
            return Err(PairingError::Windows {
                operation: "BluetoothAuthenticateDeviceEx",
                code: authentication_status,
            });
        }
    }

    let service_status = unsafe {
        BluetoothSetServiceState(
            radio_handle.0,
            &device_info,
            &WIIMOTE_HID_SERVICE,
            BLUETOOTH_SERVICE_ENABLE,
        )
    };
    if service_status != 0 && service_status != HRESULT_INVALID_ARGUMENT {
        return Err(PairingError::HidServiceEnable {
            code: service_status,
        });
    }

    Ok(PairingResult {
        device,
        was_already_authenticated,
        hid_service_enabled: true,
    })
}

struct RadioHandle(HANDLE);

impl Drop for RadioHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct RadioFindHandle(HBLUETOOTH_RADIO_FIND);

impl Drop for RadioFindHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { BluetoothFindRadioClose(self.0) };
        }
    }
}

struct DeviceFindHandle(HBLUETOOTH_DEVICE_FIND);

impl Drop for DeviceFindHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { BluetoothFindDeviceClose(self.0) };
        }
    }
}

struct AuthenticationRegistration(isize);

impl Drop for AuthenticationRegistration {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { BluetoothUnregisterAuthentication(self.0) };
        }
    }
}

struct AuthenticationContext {
    radio: HANDLE,
    device: BluetoothAddress,
    pin: [u8; 6],
    cancelled: Arc<AtomicBool>,
    response_error: AtomicU32,
    unsupported_method: AtomicI32,
    callback_progress: Mutex<CallbackProgress>,
    callback_finished: Condvar,
}

#[derive(Default)]
struct CallbackProgress {
    started: bool,
    active: usize,
}

impl AuthenticationContext {
    fn enter_callback(&self) -> CallbackCompletion<'_> {
        let mut progress = self
            .callback_progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        progress.started = true;
        progress.active += 1;
        CallbackCompletion { context: self }
    }

    fn wait_for_callback(&self) {
        let mut progress = self
            .callback_progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !progress.started || progress.active != 0 {
            progress = self
                .callback_finished
                .wait(progress)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn wait_for_idle_callbacks(&self) {
        let mut progress = self
            .callback_progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while progress.active != 0 {
            progress = self
                .callback_finished
                .wait(progress)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

struct CallbackCompletion<'a> {
    context: &'a AuthenticationContext,
}

impl Drop for CallbackCompletion<'_> {
    fn drop(&mut self) {
        let mut progress = self
            .context
            .callback_progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        progress.active -= 1;
        self.context.callback_finished.notify_all();
    }
}

unsafe extern "system" fn authentication_callback(
    parameter: *const c_void,
    callback: *const BLUETOOTH_AUTHENTICATION_CALLBACK_PARAMS,
) -> i32 {
    if parameter.is_null() || callback.is_null() {
        return 0;
    }
    let context = unsafe { &*parameter.cast::<AuthenticationContext>() };
    let callback = unsafe { &*callback };
    let _callback_completion = context.enter_callback();
    if BluetoothAddress::from_windows(callback.deviceInfo.Address) != context.device {
        return 0;
    }

    let method_supported = callback.authenticationMethod == BLUETOOTH_AUTHENTICATION_METHOD_LEGACY;
    if !method_supported {
        context
            .unsupported_method
            .store(callback.authenticationMethod, Ordering::Release);
    }
    let response_method_supported = matches!(
        callback.authenticationMethod,
        BLUETOOTH_AUTHENTICATION_METHOD_LEGACY
            | BLUETOOTH_AUTHENTICATION_METHOD_NUMERIC_COMPARISON
            | BLUETOOTH_AUTHENTICATION_METHOD_PASSKEY_NOTIFICATION
    );
    if !response_method_supported {
        return 0;
    }

    let cancelled = context.cancelled.load(Ordering::Acquire);
    let mut response = BLUETOOTH_AUTHENTICATE_RESPONSE {
        bthAddressRemote: callback.deviceInfo.Address,
        authMethod: callback.authenticationMethod,
        negativeResponse: u8::from(!method_supported || cancelled),
        ..BLUETOOTH_AUTHENTICATE_RESPONSE::default()
    };
    if method_supported {
        let mut pin_info = BLUETOOTH_PIN_INFO::default();
        pin_info.pin[..context.pin.len()].copy_from_slice(&context.pin);
        pin_info.pinLength = context.pin.len() as u8;
        response.Anonymous.pinInfo = pin_info;
    } else if callback.authenticationMethod == BLUETOOTH_AUTHENTICATION_METHOD_NUMERIC_COMPARISON {
        response.Anonymous.numericCompInfo = BLUETOOTH_NUMERIC_COMPARISON_INFO {
            NumericValue: unsafe { callback.Anonymous.Numeric_Value },
        };
    } else {
        response.Anonymous.passkeyInfo = BLUETOOTH_PASSKEY_INFO {
            passkey: unsafe { callback.Anonymous.Passkey },
        };
    }

    let status = unsafe { BluetoothSendAuthenticationResponseEx(context.radio, &response) };
    if status != 0 {
        let _ =
            context
                .response_error
                .compare_exchange(0, status, Ordering::AcqRel, Ordering::Acquire);
        0
    } else {
        1
    }
}

fn open_radio(address: BluetoothAddress) -> Result<RadioHandle, PairingError> {
    let mut radio_handle: HANDLE = std::ptr::null_mut();
    let parameters = BLUETOOTH_FIND_RADIO_PARAMS {
        dwSize: std::mem::size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32,
    };
    unsafe { SetLastError(0) };
    let find_handle = unsafe { BluetoothFindFirstRadio(&parameters, &mut radio_handle) };
    if find_handle.is_null() {
        let code = unsafe { GetLastError() };
        if code == 0 || code == ERROR_NO_MORE_ITEMS {
            return Err(PairingError::NoBluetoothRadio);
        }
        return Err(PairingError::Windows {
            operation: "BluetoothFindFirstRadio",
            code,
        });
    }

    let find_handle = RadioFindHandle(find_handle);
    loop {
        if radio_handle.is_null() {
            return Err(PairingError::NoBluetoothRadio);
        }
        let candidate = RadioHandle(radio_handle);
        let info = read_radio_info(candidate.0)?;
        if info.address == address {
            return Ok(candidate);
        }

        radio_handle = std::ptr::null_mut();
        unsafe { SetLastError(0) };
        if unsafe { BluetoothFindNextRadio(find_handle.0, &mut radio_handle) } == 0 {
            let code = unsafe { GetLastError() };
            if code != 0 && code != ERROR_NO_MORE_ITEMS {
                return Err(PairingError::Windows {
                    operation: "BluetoothFindNextRadio",
                    code,
                });
            }
            return Err(PairingError::RadioNotFound(address));
        }
    }
}

fn read_radio_info(handle: HANDLE) -> Result<Radio, PairingError> {
    let mut info = BLUETOOTH_RADIO_INFO {
        dwSize: std::mem::size_of::<BLUETOOTH_RADIO_INFO>() as u32,
        ..BLUETOOTH_RADIO_INFO::default()
    };
    let status = unsafe { BluetoothGetRadioInfo(handle, &mut info) };
    if status != 0 {
        return Err(PairingError::Windows {
            operation: "BluetoothGetRadioInfo",
            code: status,
        });
    }
    Ok(Radio {
        address: BluetoothAddress::from_windows(info.address),
        name: decode_wide_name(&info.szName),
    })
}

fn empty_device_info() -> BLUETOOTH_DEVICE_INFO {
    BLUETOOTH_DEVICE_INFO {
        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
        ..BLUETOOTH_DEVICE_INFO::default()
    }
}

fn device_from_info(info: &BLUETOOTH_DEVICE_INFO) -> Device {
    Device {
        address: BluetoothAddress::from_windows(info.Address),
        name: decode_wide_name(&info.szName),
        class_of_device: info.ulClassofDevice,
        connected: info.fConnected != 0,
        remembered: info.fRemembered != 0,
        authenticated: info.fAuthenticated != 0,
    }
}

fn decode_wide_name(name: &[u16]) -> String {
    let end = name
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(name.len());
    String::from_utf16_lossy(&name[..end])
}
