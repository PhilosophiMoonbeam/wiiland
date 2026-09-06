//! Direct Linux uinput output with deterministic backend seams.
use std::io;
use std::mem::size_of;
use std::os::fd::RawFd;
use std::path::Path;
use std::sync::{Arc, Mutex};

use wiiland_core::mapping::{
    CONTROLLER_CAPABILITIES, DESKTOP_CAPABILITIES, VirtualCapabilities, axis_info,
};

const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
const UI_SET_ABSBIT: libc::c_ulong = 0x4004_5567;
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const BUS_BLUETOOTH: u16 = 0x0005;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct UinputUserDev {
    name: [u8; 80],
    id: InputId,
    ff_effects_max: i32,
    absmax: [i32; 64],
    absmin: [i32; 64],
    absfuzz: [i32; 64],
    absflat: [i32; 64],
}
impl Default for UinputUserDev {
    fn default() -> Self {
        Self {
            name: [0; 80],
            id: InputId::default(),
            ff_effects_max: 0,
            absmax: [0; 64],
            absmin: [0; 64],
            absfuzz: [0; 64],
            absflat: [0; 64],
        }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct InputEvent {
    pub time: libc::timeval,
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}
impl PartialEq for InputEvent {
    fn eq(&self, other: &Self) -> bool {
        self.time.tv_sec == other.time.tv_sec
            && self.time.tv_usec == other.time.tv_usec
            && self.type_ == other.type_
            && self.code == other.code
            && self.value == other.value
    }
}
impl Eq for InputEvent {}

pub trait Backend: Send {
    fn open(&mut self, path: &Path) -> io::Result<RawFd>;
    fn ioctl(&mut self, fd: RawFd, request: libc::c_ulong, arg: libc::c_int) -> io::Result<()>;
    fn write(&mut self, fd: RawFd, data: &[u8]) -> io::Result<usize>;
    fn close(&mut self, fd: RawFd);
}

#[derive(Clone, Copy, Default)]
pub struct SystemBackend;
impl Backend for SystemBackend {
    fn open(&mut self, path: &Path) -> io::Result<RawFd> {
        use std::ffi::CString;
        let c = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(fd)
        }
    }
    fn ioctl(&mut self, fd: RawFd, request: libc::c_ulong, arg: libc::c_int) -> io::Result<()> {
        let r = unsafe { libc::ioctl(fd, request, arg) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    fn write(&mut self, fd: RawFd, data: &[u8]) -> io::Result<usize> {
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
    fn close(&mut self, fd: RawFd) {
        unsafe {
            libc::close(fd);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordingOp {
    Open(String),
    Ioctl {
        request: libc::c_ulong,
        arg: libc::c_int,
    },
    Write(Vec<u8>),
    Destroy,
    Close,
}
#[derive(Clone, Default)]
pub struct RecordingBackend {
    pub ops: Arc<Mutex<Vec<RecordingOp>>>,
    pub next_fd: RawFd,
    pub write_result: Option<io::ErrorKind>,
    pub short_write: Option<usize>,
    pub fail_ioctl: Option<libc::c_ulong>,
}
impl RecordingBackend {
    pub fn new() -> Self {
        Self {
            next_fd: 41,
            ..Self::default()
        }
    }
    pub fn operations(&self) -> Vec<RecordingOp> {
        self.ops.lock().map(|ops| ops.clone()).unwrap_or_default()
    }
}
impl Backend for RecordingBackend {
    fn open(&mut self, path: &Path) -> io::Result<RawFd> {
        self.ops
            .lock()
            .map_err(|_| io::Error::other("recording lock poisoned"))?
            .push(RecordingOp::Open(path.display().to_string()));
        Ok(self.next_fd)
    }
    fn ioctl(&mut self, fd: RawFd, request: libc::c_ulong, arg: libc::c_int) -> io::Result<()> {
        let _ = fd;
        let op = if request == UI_DEV_DESTROY {
            RecordingOp::Destroy
        } else {
            RecordingOp::Ioctl { request, arg }
        };
        self.ops
            .lock()
            .map_err(|_| io::Error::other("recording lock poisoned"))?
            .push(op);
        if self.fail_ioctl == Some(request) {
            Err(io::Error::from_raw_os_error(libc::EIO))
        } else {
            Ok(())
        }
    }
    fn write(&mut self, fd: RawFd, data: &[u8]) -> io::Result<usize> {
        let _ = fd;
        if let Some(k) = self.write_result {
            return Err(io::Error::from_raw_os_error(match k {
                io::ErrorKind::WouldBlock => libc::EAGAIN,
                io::ErrorKind::Interrupted => libc::EINTR,
                _ => libc::EIO,
            }));
        }
        self.ops
            .lock()
            .map_err(|_| io::Error::other("recording lock poisoned"))?
            .push(RecordingOp::Write(data.to_vec()));
        Ok(self.short_write.unwrap_or(data.len()))
    }
    fn close(&mut self, fd: RawFd) {
        let _ = fd;
        if let Ok(mut ops) = self.ops.lock() {
            ops.push(RecordingOp::Close);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VirtualKind {
    Controller,
    Desktop,
}
impl VirtualKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Controller => "WiiLand Virtual Controller",
            Self::Desktop => "WiiLand Virtual Desktop",
        }
    }
    pub fn capabilities(self) -> VirtualCapabilities {
        match self {
            Self::Controller => CONTROLLER_CAPABILITIES,
            Self::Desktop => DESKTOP_CAPABILITIES,
        }
    }
}

pub struct VirtualDevice<B: Backend = SystemBackend> {
    backend: B,
    fd: RawFd,
    created: bool,
    kind: VirtualKind,
}
impl VirtualDevice<SystemBackend> {
    pub fn controller(path: impl AsRef<Path>) -> Result<Self, i32> {
        Self::new(path, VirtualKind::Controller)
    }
    pub fn desktop(path: impl AsRef<Path>) -> Result<Self, i32> {
        Self::new(path, VirtualKind::Desktop)
    }
    pub fn new(path: impl AsRef<Path>, kind: VirtualKind) -> Result<Self, i32> {
        Self::with_backend(path, kind, SystemBackend)
    }
}
impl<B: Backend> VirtualDevice<B> {
    pub fn with_backend(
        path: impl AsRef<Path>,
        kind: VirtualKind,
        mut backend: B,
    ) -> Result<Self, i32> {
        let fd = backend.open(path.as_ref()).map_err(errno)?;
        let mut out = Self {
            backend,
            fd,
            created: false,
            kind,
        };
        // `out` owns the descriptor from this point, including setup failures.
        out.configure()?;
        out.backend.ioctl(fd, UI_DEV_CREATE, 0).map_err(errno)?;
        out.created = true;
        Ok(out)
    }
    fn configure(&mut self) -> Result<(), i32> {
        let caps = self.kind.capabilities();
        self.ioctl(UI_SET_EVBIT, EV_KEY as i32)?;
        for &code in caps.keys {
            self.ioctl(UI_SET_KEYBIT, code as i32)?;
        }
        if !caps.axes.is_empty() {
            self.ioctl(UI_SET_EVBIT, EV_ABS as i32)?;
            for &code in caps.axes {
                self.ioctl(UI_SET_ABSBIT, code as i32)?;
            }
        }
        if !caps.rels.is_empty() {
            self.ioctl(UI_SET_EVBIT, EV_REL as i32)?;
            for &code in caps.rels {
                self.ioctl(UI_SET_RELBIT, code as i32)?;
            }
        }
        let mut u = UinputUserDev::default();
        let bytes = self.kind.name().as_bytes();
        u.name[..bytes.len().min(79)].copy_from_slice(&bytes[..bytes.len().min(79)]);
        u.id = InputId {
            bustype: BUS_BLUETOOTH,
            vendor: 0x057e,
            product: 0x0337,
            version: 1,
        };
        for &code in caps.axes {
            if let Some(info) = axis_info(code) {
                u.absmin[code as usize] = info.minimum;
                u.absmax[code as usize] = info.maximum;
                u.absflat[code as usize] = info.flat;
                u.absfuzz[code as usize] = info.fuzz;
            }
        }
        self.write_struct(&u)
    }
    fn ioctl(&mut self, request: libc::c_ulong, arg: libc::c_int) -> Result<(), i32> {
        self.backend.ioctl(self.fd, request, arg).map_err(errno)
    }
    fn write_struct<T>(&mut self, value: &T) -> Result<(), i32> {
        let bytes =
            unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
        self.write_exact(bytes)
    }
    fn write_exact(&mut self, bytes: &[u8]) -> Result<(), i32> {
        match self.backend.write(self.fd, bytes) {
            Ok(n) if n == bytes.len() => Ok(()),
            Ok(_) => Err(-libc::EIO),
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => self.write_exact(bytes),
            Err(e) => Err(errno(e)),
        }
    }
    pub fn kind(&self) -> VirtualKind {
        self.kind
    }
    pub fn fd(&self) -> RawFd {
        self.fd
    }
    pub fn emit_event(&mut self, type_: u16, code: u16, value: i32) -> Result<(), i32> {
        let e = InputEvent {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };
        self.write_struct(&e)
    }
    pub fn emit_key(&mut self, code: u16, state: u32) -> Result<(), i32> {
        self.emit_event(EV_KEY, code, state.min(2) as i32)?;
        self.syn()
    }
    pub fn emit_abs(&mut self, code: u16, value: i32) -> Result<(), i32> {
        self.emit_event(EV_ABS, code, value)
    }
    pub fn emit_rel(&mut self, code: u16, value: i32) -> Result<(), i32> {
        if value != 0 {
            self.emit_event(EV_REL, code, value)
        } else {
            Ok(())
        }
    }
    pub fn syn(&mut self) -> Result<(), i32> {
        self.emit_event(EV_SYN, SYN_REPORT, 0)
    }
}
impl<B: Backend> Drop for VirtualDevice<B> {
    fn drop(&mut self) {
        if self.created {
            let _ = self.backend.ioctl(self.fd, UI_DEV_DESTROY, 0);
        }
        self.backend.close(self.fd);
    }
}
fn errno(e: io::Error) -> i32 {
    -e.raw_os_error().unwrap_or(libc::EIO)
}

pub const UINPUT_PATH: &str = "/dev/uinput";
