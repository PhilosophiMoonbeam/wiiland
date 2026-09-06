//! Signal wakeup using a nonblocking self-pipe.
use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

static WRITE_FD: AtomicI32 = AtomicI32::new(-1);
static STOP: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Roll back exclusive ownership when signal installation fails partway through.
struct InstallationGuard(bool);
impl Drop for InstallationGuard {
    fn drop(&mut self) {
        if self.0 {
            INSTALLED.store(false, Ordering::Release);
        }
    }
}

/// Workers inherit blocked daemon signals before their first instruction. Only
/// the reactor thread may execute the handler, including during fd teardown.
pub(crate) fn spawn_worker<F, T>(name: &str, task: F) -> io::Result<std::thread::JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let previous =
        block_signals(&handled_signal_set()).map_err(|code| io::Error::from_raw_os_error(-code))?;
    let worker = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(task);
    restore_signal_mask(&previous).map_err(|code| io::Error::from_raw_os_error(-code))?;
    worker
}

extern "C" fn on_signal(signo: libc::c_int) {
    let errno_location = unsafe { libc::__errno_location() };
    let saved_errno = unsafe { *errno_location };
    STOP.store(true, Ordering::Release);
    let fd = WRITE_FD.load(Ordering::Acquire);
    if fd >= 0 {
        let byte = signo as u8;
        unsafe {
            libc::write(fd, (&byte as *const u8).cast(), 1);
        }
    }
    unsafe {
        *errno_location = saved_errno;
    }
}

pub struct SignalPipe {
    read_fd: RawFd,
    write_fd: RawFd,
    old_int: libc::sigaction,
    old_term: libc::sigaction,
    active: bool,
}
impl SignalPipe {
    pub fn install() -> Result<Self, i32> {
        INSTALLED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| -libc::EBUSY)?;
        let mut ownership = InstallationGuard(true);
        let mut fds = [-1; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } < 0 {
            return Err(errno());
        }

        let signal_set = handled_signal_set();
        let previous_mask = match block_signals(&signal_set) {
            Ok(mask) => mask,
            Err(error) => {
                close_pipe(fds);
                return Err(error);
            }
        };

        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_signal as *const () as usize;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        let mut old_int: libc::sigaction = unsafe { std::mem::zeroed() };
        let mut old_term = old_int;

        publish_handler_state(fds[1]);
        if unsafe { libc::sigaction(libc::SIGINT, &action, &mut old_int) } < 0 {
            let error = errno();
            unpublish_handler_state();
            close_pipe(fds);
            let _ = restore_signal_mask(&previous_mask);
            return Err(error);
        }
        if unsafe { libc::sigaction(libc::SIGTERM, &action, &mut old_term) } < 0 {
            let error = errno();
            unsafe {
                libc::sigaction(libc::SIGINT, &old_int, std::ptr::null_mut());
            }
            unpublish_handler_state();
            close_pipe(fds);
            let _ = restore_signal_mask(&previous_mask);
            return Err(error);
        }
        if let Err(error) = restore_signal_mask(&previous_mask) {
            unpublish_handler_state();
            unsafe {
                libc::sigaction(libc::SIGINT, &old_int, std::ptr::null_mut());
                libc::sigaction(libc::SIGTERM, &old_term, std::ptr::null_mut());
            }
            close_pipe(fds);
            return Err(error);
        }

        ownership.0 = false;
        Ok(Self {
            read_fd: fds[0],
            write_fd: fds[1],
            old_int,
            old_term,
            active: true,
        })
    }
    pub fn fds(&self) -> (RawFd, RawFd) {
        (self.read_fd, self.write_fd)
    }
    pub fn read_fd(&self) -> RawFd {
        self.read_fd
    }
    pub fn requested(&self) -> bool {
        STOP.load(Ordering::Acquire)
    }
    pub fn drain(&self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe { libc::read(self.read_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
                continue;
            }
            if n < 0 && matches!(io::Error::last_os_error().raw_os_error(), Some(libc::EINTR)) {
                continue;
            }
            if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock {
                return Ok(out);
            }
            if n == 0 {
                return Ok(out);
            }
            return Err(io::Error::last_os_error());
        }
    }
    fn close(&mut self) {
        if !self.active {
            return;
        }
        let signal_set = handled_signal_set();
        let Ok(previous_mask) = block_signals(&signal_set) else {
            return;
        };

        self.active = false;
        unpublish_handler_state();
        unsafe {
            libc::sigaction(libc::SIGINT, &self.old_int, std::ptr::null_mut());
            libc::sigaction(libc::SIGTERM, &self.old_term, std::ptr::null_mut());
        }
        close_pipe([self.read_fd, self.write_fd]);
        let _ = restore_signal_mask(&previous_mask);
        INSTALLED.store(false, Ordering::Release);
    }
}
impl Drop for SignalPipe {
    fn drop(&mut self) {
        self.close()
    }
}

fn handled_signal_set() -> libc::sigset_t {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
    }
    set
}

fn block_signals(set: &libc::sigset_t) -> Result<libc::sigset_t, i32> {
    let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, set, &mut previous) };
    if result != 0 {
        Err(-result)
    } else {
        Ok(previous)
    }
}

fn restore_signal_mask(previous: &libc::sigset_t) -> Result<(), i32> {
    let result =
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, previous, std::ptr::null_mut()) };
    if result != 0 { Err(-result) } else { Ok(()) }
}

fn publish_handler_state(write_fd: RawFd) {
    STOP.store(false, Ordering::Release);
    WRITE_FD.store(write_fd, Ordering::Release);
}

fn unpublish_handler_state() {
    WRITE_FD.store(-1, Ordering::Release);
}

fn close_pipe(fds: [RawFd; 2]) {
    unsafe {
        libc::close(fds[0]);
        libc::close(fds[1]);
    }
}

fn errno() -> i32 {
    -io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workers_inherit_blocked_signals_and_parent_mask_is_restored() {
        fn mask() -> [i32; 2] {
            let mut empty: libc::sigset_t = unsafe { std::mem::zeroed() };
            unsafe {
                libc::sigemptyset(&mut empty);
            }
            let current = block_signals(&empty).unwrap();
            unsafe {
                [
                    libc::sigismember(&current, libc::SIGINT),
                    libc::sigismember(&current, libc::SIGTERM),
                ]
            }
        }
        let before = mask();
        let inherited = spawn_worker("wiiland-mask-test", mask)
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(inherited, [1, 1]);
        assert_eq!(mask(), before);
    }

    #[test]
    fn handled_signal_set_contains_int_and_term() {
        let set = handled_signal_set();
        assert_eq!(unsafe { libc::sigismember(&set, libc::SIGINT) }, 1);
        assert_eq!(unsafe { libc::sigismember(&set, libc::SIGTERM) }, 1);
    }

    #[test]
    fn handler_state_publication_and_errno_preservation() {
        STOP.store(true, Ordering::Release);
        unpublish_handler_state();

        publish_handler_state(123);

        assert!(!STOP.load(Ordering::Acquire));
        assert_eq!(WRITE_FD.load(Ordering::Acquire), 123);
        unpublish_handler_state();

        unsafe {
            *libc::__errno_location() = libc::EDOM;
        }
        on_signal(libc::SIGINT);

        assert!(STOP.load(Ordering::Acquire));
        assert_eq!(unsafe { *libc::__errno_location() }, libc::EDOM);
        STOP.store(false, Ordering::Release);
    }
}
