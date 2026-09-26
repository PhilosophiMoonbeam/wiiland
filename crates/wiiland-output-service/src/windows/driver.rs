use crate::protocol::{MAX_REPORT_PAYLOAD, OutputLease, REPORT_LAYOUT_VERSION, ReportId};
use crate::windows::security::OwnedHandle;
use std::io;
use std::mem::{offset_of, size_of};
use std::ptr;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS, GetLastError,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING};
use windows_sys::Win32::System::IO::DeviceIoControl;

const FILE_DEVICE_UNKNOWN: u32 = 0x22;
const FILE_WRITE_ACCESS: u32 = 0x2;
const METHOD_BUFFERED: u32 = 0;
const IOCTL_CREATE: u32 = ctl_code(0x800);
const IOCTL_REPORT: u32 = ctl_code(0x801);
const IOCTL_DESTROY: u32 = ctl_code(0x802);
const ABI_VERSION: u32 = 2;

const fn ctl_code(function: u32) -> u32 {
    (FILE_DEVICE_UNKNOWN << 16) | (FILE_WRITE_ACCESS << 14) | (function << 2) | METHOD_BUFFERED
}

#[repr(C)]
struct CreateInputV2 {
    size: u32,
    version: u32,
    slot: u32,
    flags: u32,
}

#[repr(C)]
struct CreateResultV2 {
    size: u32,
    version: u32,
    slot: u32,
    generation: u32,
    report_layout_version: u32,
}

#[repr(C)]
struct DestroyInputV2 {
    size: u32,
    version: u32,
    slot: u32,
    generation: u32,
}

#[repr(C)]
struct ReportInputV2 {
    size: u32,
    version: u32,
    slot: u32,
    generation: u32,
    report_id: u8,
    payload_len: u8,
    reserved: [u8; 2],
    payload: [u8; MAX_REPORT_PAYLOAD],
}

const _: [(); 16] = [(); size_of::<CreateInputV2>()];
const _: [(); 20] = [(); size_of::<CreateResultV2>()];
const _: [(); 16] = [(); size_of::<DestroyInputV2>()];
const _: [(); 108] = [(); size_of::<ReportInputV2>()];

pub(crate) struct DeviceHandle(OwnedHandle);

impl DeviceHandle {
    pub(crate) fn open() -> io::Result<Self> {
        let class = windows_sys::core::GUID {
            data1: 0x4757_6e0f,
            data2: 0x990a,
            data3: 0x4c5e,
            data4: [0xbd, 0xbd, 0xba, 0xda, 0x13, 0xaf, 0x75, 0x42],
        };
        let set = unsafe {
            SetupDiGetClassDevsW(
                &class,
                ptr::null(),
                ptr::null_mut(),
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        };
        if set == INVALID_HANDLE_VALUE as HDEVINFO {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "cannot enumerate the WiiLand virtual HID interface; install and start the signed wiiland-vhid driver package first: {error}"
                ),
            ));
        }
        let device_set = DeviceInfoSet(set);
        let mut index = 0u32;
        let mut last_open_error = None;
        loop {
            let mut interface = SP_DEVICE_INTERFACE_DATA {
                cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..SP_DEVICE_INTERFACE_DATA::default()
            };
            if unsafe {
                SetupDiEnumDeviceInterfaces(
                    device_set.0,
                    ptr::null(),
                    &class,
                    index,
                    &mut interface,
                )
            } == 0
            {
                let error = unsafe { GetLastError() };
                if error == ERROR_NO_MORE_ITEMS {
                    break;
                }
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            index = index.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "VHID interface count overflow")
            })?;
            let mut required = 0u32;
            let result = unsafe {
                SetupDiGetDeviceInterfaceDetailW(
                    device_set.0,
                    &interface,
                    ptr::null_mut(),
                    0,
                    &mut required,
                    ptr::null_mut(),
                )
            };
            if result != 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
                return Err(io::Error::last_os_error());
            }
            let detail_offset = offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
            let detail_size = usize::try_from(required).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "VHID path size overflow")
            })?;
            if detail_size < detail_offset + size_of::<u16>() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows returned a truncated VHID interface path",
                ));
            }
            let mut storage = vec![0usize; detail_size.div_ceil(size_of::<usize>())];
            let detail = storage
                .as_mut_ptr()
                .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
            unsafe {
                (*detail).cbSize = if size_of::<usize>() == 8 { 8 } else { 6 };
            }
            if unsafe {
                SetupDiGetDeviceInterfaceDetailW(
                    device_set.0,
                    &interface,
                    detail,
                    required,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let path_words = (detail_size - detail_offset) / size_of::<u16>();
            let path =
                unsafe { std::slice::from_raw_parts((*detail).DevicePath.as_ptr(), path_words) };
            let path_len = path
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(path.len());
            if path_len == 0 {
                continue;
            }
            let handle = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    windows_sys::Win32::Foundation::GENERIC_READ
                        | windows_sys::Win32::Foundation::GENERIC_WRITE,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    ptr::null_mut(),
                )
            };
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                return Ok(Self(OwnedHandle(handle)));
            }
            let error = unsafe { GetLastError() };
            last_open_error = Some(error);
        }

        match last_open_error {
            Some(ERROR_ACCESS_DENIED) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "access to the WiiLand VHID device was denied; run WiiLandOutput as LocalSystem and install it with `sc.exe sidtype WiiLandOutput unrestricted` so the INF service-SID ACL grants access",
            )),
            Some(error) => Err(io::Error::from_raw_os_error(error as i32)),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the WiiLand VHID interface is not present; install and start the signed wiiland-vhid driver package first",
            )),
        }
    }

    pub(crate) fn create_slot(&self, slot: u32) -> io::Result<OutputLease> {
        let input = CreateInputV2 {
            size: size_of::<CreateInputV2>() as u32,
            version: ABI_VERSION,
            slot,
            flags: 0,
        };
        let mut output = CreateResultV2 {
            size: 0,
            version: 0,
            slot: 0,
            generation: 0,
            report_layout_version: 0,
        };
        let mut returned = 0u32;
        self.device_io(
            IOCTL_CREATE,
            (&input as *const CreateInputV2).cast(),
            size_of::<CreateInputV2>() as u32,
            (&mut output as *mut CreateResultV2).cast(),
            size_of::<CreateResultV2>() as u32,
            &mut returned,
        )?;
        if returned as usize != size_of::<CreateResultV2>()
            || output.size as usize != size_of::<CreateResultV2>()
            || output.version != ABI_VERSION
            || output.slot != slot
            || output.generation == 0
            || output.report_layout_version != REPORT_LAYOUT_VERSION
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "VHID CREATE returned a result outside the v2 ABI",
            ));
        }
        Ok(OutputLease {
            slot: output.slot,
            generation: output.generation,
        })
    }

    pub(crate) fn report_payload(
        &self,
        lease: OutputLease,
        report_id: u8,
        payload: &[u8],
    ) -> io::Result<()> {
        if payload.len() > MAX_REPORT_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "VHID report payload exceeds the v2 ABI limit",
            ));
        }
        let payload_len = u8::try_from(payload.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "VHID report payload is too large",
            )
        })?;
        let mut input = ReportInputV2 {
            size: size_of::<ReportInputV2>() as u32,
            version: ABI_VERSION,
            slot: lease.slot,
            generation: lease.generation,
            report_id,
            payload_len,
            reserved: [0; 2],
            payload: [0; MAX_REPORT_PAYLOAD],
        };
        input.payload[..payload.len()].copy_from_slice(payload);
        let mut returned = 0u32;
        self.device_io(
            IOCTL_REPORT,
            (&input as *const ReportInputV2).cast(),
            size_of::<ReportInputV2>() as u32,
            ptr::null_mut(),
            0,
            &mut returned,
        )
    }

    pub(crate) fn destroy_slot(&self, lease: OutputLease) -> io::Result<()> {
        let input = DestroyInputV2 {
            size: size_of::<DestroyInputV2>() as u32,
            version: ABI_VERSION,
            slot: lease.slot,
            generation: lease.generation,
        };
        let mut returned = 0u32;
        self.device_io(
            IOCTL_DESTROY,
            (&input as *const DestroyInputV2).cast(),
            size_of::<DestroyInputV2>() as u32,
            ptr::null_mut(),
            0,
            &mut returned,
        )
    }

    pub(crate) fn neutralize_and_destroy(&self, lease: OutputLease) -> io::Result<()> {
        const GAMEPAD_NEUTRAL: [u8; 17] = [0, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        const SUPPLEMENTAL_NEUTRAL: [u8; 88] = [0; 88];
        let _ = self.report_payload(lease, ReportId::Gamepad as u8, &GAMEPAD_NEUTRAL);
        let _ = self.report_payload(
            lease,
            ReportId::SupplementalAxes as u8,
            &SUPPLEMENTAL_NEUTRAL,
        );
        self.destroy_slot(lease)
    }

    fn device_io(
        &self,
        code: u32,
        input: *const std::ffi::c_void,
        input_size: u32,
        output: *mut std::ffi::c_void,
        output_size: u32,
        returned: &mut u32,
    ) -> io::Result<()> {
        if unsafe {
            DeviceIoControl(
                self.0.0,
                code,
                input,
                input_size,
                output,
                output_size,
                returned,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

struct DeviceInfoSet(HDEVINFO);

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}
