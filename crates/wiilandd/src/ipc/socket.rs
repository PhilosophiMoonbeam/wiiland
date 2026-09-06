//! Private socket ownership and race-resistant filesystem operations.
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
static QUARANTINE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(super) struct SocketLock {
    pub(super) _file: File,
}

#[derive(Debug)]
pub(super) struct PinnedParent {
    pub(super) file: File,
    pub(super) public_path: PathBuf,
    pub(super) inode: (u64, u64),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) struct EntryMetadata {
    pub(super) inode: (u64, u64),
    pub(super) mode: libc::mode_t,
    pub(super) uid: libc::uid_t,
}

impl EntryMetadata {
    pub(super) fn is_socket(self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFSOCK
    }
}

#[derive(Debug)]
pub(super) struct BoundSocketGuard<'a> {
    pub(super) parent: &'a PinnedParent,
    pub(super) name: OsString,
    pub(super) inode: (u64, u64),
    pub(super) armed: bool,
}

impl<'a> BoundSocketGuard<'a> {
    pub(super) fn new(parent: &'a PinnedParent, name: &OsStr, inode: (u64, u64)) -> Self {
        Self {
            parent,
            name: name.to_os_string(),
            inode,
            armed: true,
        }
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BoundSocketGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = remove_expected_entry(self.parent, &self.name, self.inode);
        }
    }
}

pub(super) fn setup_bound_socket(
    parent: &PinnedParent,
    name: &OsStr,
    listener: &UnixListener,
    inode: (u64, u64),
) -> io::Result<()> {
    let socket = parent.open_entry(name, libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0)?;
    let meta = socket.metadata()?;
    ensure_owner(&meta, "IPC socket")?;
    if !meta.file_type().is_socket() || (meta.dev(), meta.ino()) != inode {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "IPC socket path changed during setup",
        ));
    }
    // SAFETY: `socket` is a live O_PATH descriptor, the empty C string is valid,
    // and fchmodat2 does not retain either pointer after the syscall.
    let result = unsafe {
        libc::syscall(
            libc::SYS_fchmodat2,
            socket.as_raw_fd(),
            c"".as_ptr(),
            0o600,
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    listener.set_nonblocking(true)?;
    let meta = parent.entry_metadata(name)?;
    if !meta.is_socket() || meta.inode != inode || meta.mode & 0o777 != 0o600 {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "IPC socket path changed during setup",
        ));
    }
    Ok(())
}

impl PinnedParent {
    pub(super) fn open(public_path: PathBuf) -> io::Result<Self> {
        let path = path_c_string(&public_path)?;
        // SAFETY: `path` is a valid NUL-terminated pathname and open does not
        // retain its pointer. The returned descriptor is uniquely owned below.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: open returned a fresh descriptor which is transferred once.
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata()?;
        validate_private_directory(&meta, "IPC socket parent")?;
        let inode = (meta.dev(), meta.ino());
        Ok(Self {
            file,
            public_path,
            inode,
        })
    }

    pub(super) fn verify_public_identity(&self) -> io::Result<()> {
        let current = Self::open(self.public_path.clone()).map_err(|error| {
            io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("IPC socket parent changed during bind: {error}"),
            )
        })?;
        if current.inode != self.inode {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "IPC socket parent changed during bind",
            ));
        }
        Ok(())
    }

    pub(super) fn entry_path(&self, name: &OsStr) -> PathBuf {
        let mut path = PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()));
        path.push(name);
        path
    }

    pub(super) fn entry_metadata(&self, name: &OsStr) -> io::Result<EntryMetadata> {
        let name = os_c_string(name)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `name` is a valid C string, `stat` points to writable storage,
        // and fstatat initializes it completely on success.
        let result = unsafe {
            libc::fstatat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful fstatat above initialized every field.
        let stat = unsafe { stat.assume_init() };
        Ok(EntryMetadata {
            inode: (stat.st_dev, stat.st_ino),
            mode: stat.st_mode,
            uid: stat.st_uid,
        })
    }

    pub(super) fn open_entry(
        &self,
        name: &OsStr,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> io::Result<File> {
        let name = os_c_string(name)?;
        // SAFETY: `name` is a valid C string, the directory descriptor remains
        // open for the call, and openat does not retain the pointer.
        let fd = unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags, mode) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a fresh descriptor which is transferred once.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

pub(super) fn path_c_string(path: &Path) -> io::Result<CString> {
    os_c_string(path.as_os_str())
}

pub(super) fn os_c_string(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "IPC path contains a NUL byte"))
}

pub(super) fn socket_name(path: &Path) -> io::Result<&OsStr> {
    path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPC socket path has no file name",
        )
    })
}

pub(super) fn remove_expected_entry(
    parent: &PinnedParent,
    name: &OsStr,
    inode: (u64, u64),
) -> io::Result<()> {
    remove_expected_entry_with_hook(parent, name, inode, || {})
}

pub(super) fn remove_expected_entry_with_hook<F>(
    parent: &PinnedParent,
    name: &OsStr,
    inode: (u64, u64),
    before_capture: F,
) -> io::Result<()>
where
    F: FnOnce(),
{
    before_capture();
    let Some(quarantine) = capture_entry(parent, name, inode)? else {
        return Ok(());
    };
    let captured = match parent.entry_metadata(&quarantine) {
        Ok(meta) => meta,
        Err(error) => {
            let _ = restore_capture(parent, &quarantine, name);
            return Err(error);
        }
    };
    if !captured.is_socket() || captured.inode != inode {
        let restored = restore_capture(parent, &quarantine, name);
        let detail = match restored {
            Ok(()) => "replacement was restored".to_string(),
            Err(error) => format!("replacement is preserved in quarantine: {error}"),
        };
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("IPC socket entry changed during cleanup; {detail}"),
        ));
    }

    let quarantine_c = os_c_string(&quarantine)?;
    // SAFETY: `quarantine_c` is a valid C string and the pinned directory
    // descriptor remains live for the duration of unlinkat.
    if unsafe { libc::unlinkat(parent.file.as_raw_fd(), quarantine_c.as_ptr(), 0) } != 0 {
        let error = io::Error::last_os_error();
        let _ = restore_capture(parent, &quarantine, name);
        return Err(error);
    }
    Ok(())
}

pub(super) fn capture_entry(
    parent: &PinnedParent,
    name: &OsStr,
    inode: (u64, u64),
) -> io::Result<Option<OsString>> {
    let name_c = os_c_string(name)?;
    for _ in 0..64 {
        let sequence = QUARANTINE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let quarantine = OsString::from(format!(
            ".wiilandd-quarantine-{:x}-{:x}-{:x}-{:x}",
            std::process::id(),
            inode.0,
            inode.1,
            sequence
        ));
        let quarantine_c = os_c_string(&quarantine)?;
        // SAFETY: both names are valid C strings, both directory descriptors
        // are the same live pinned descriptor, and renameat2 retains no pointers.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                parent.file.as_raw_fd(),
                name_c.as_ptr(),
                parent.file.as_raw_fd(),
                quarantine_c.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            return Ok(Some(quarantine));
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EEXIST) => continue,
            Some(libc::ENOENT) => return Ok(None),
            _ => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a private IPC cleanup quarantine name",
    ))
}

pub(super) fn restore_capture(
    parent: &PinnedParent,
    quarantine: &OsStr,
    name: &OsStr,
) -> io::Result<()> {
    let quarantine = os_c_string(quarantine)?;
    let name = os_c_string(name)?;
    // SAFETY: both names are valid C strings, both directory descriptors are
    // the same live pinned descriptor, and renameat2 retains no pointers.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            parent.file.as_raw_fd(),
            quarantine.as_ptr(),
            parent.file.as_raw_fd(),
            name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn ensure_private_parent(parent: &Path) -> io::Result<()> {
    match fs::symlink_metadata(parent) {
        Ok(meta) => validate_private_directory(&meta, "IPC socket parent"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let grandparent = parent
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let grandparent_meta = fs::symlink_metadata(grandparent)?;
            validate_private_directory(&grandparent_meta, "IPC socket grandparent")?;

            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(parent) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            let parent_meta = fs::symlink_metadata(parent)?;
            validate_private_directory(&parent_meta, "IPC socket parent")
        }
        Err(error) => Err(error),
    }
}

pub(super) fn validate_private_directory(meta: &fs::Metadata, what: &str) -> io::Result<()> {
    if !meta.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{what} is not a directory"),
        ));
    }
    ensure_owner(meta, what)?;
    if meta.permissions().mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{what} is not mode 0700"),
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn socket_lock_path(path: &Path) -> io::Result<PathBuf> {
    Ok(path.with_file_name(socket_lock_name(socket_name(path)?)))
}

pub(super) fn socket_lock_name(name: &OsStr) -> OsString {
    let mut lock_name = name.to_os_string();
    lock_name.push(".lock");
    lock_name
}

pub(super) fn acquire_socket_lock(parent: &PinnedParent, name: &OsStr) -> io::Result<SocketLock> {
    let lock_name = socket_lock_name(name);
    let create_flags = libc::O_RDWR
        | libc::O_CREAT
        | libc::O_EXCL
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | libc::O_CLOEXEC;
    let (file, created) = match parent.open_entry(&lock_name, create_flags, 0o600) {
        Ok(file) => (file, true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (
            parent.open_entry(
                &lock_name,
                libc::O_RDWR | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                0,
            )?,
            false,
        ),
        Err(error) => return Err(error),
    };

    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC socket lock is not a regular file",
        ));
    }
    ensure_owner(&meta, "IPC socket lock")?;
    if created {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let meta = file.metadata()?;
    if meta.permissions().mode() & 0o777 != 0o600 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC socket lock is not mode 0600",
        ));
    }

    // SAFETY: `file` owns a live descriptor and flock only inspects that value.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "IPC socket startup lock is already held",
            ));
        }
        return Err(error);
    }

    let named = parent.entry_metadata(&lock_name)?;
    if named.inode != (meta.dev(), meta.ino()) || named.mode & libc::S_IFMT != libc::S_IFREG {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "IPC socket lock changed during acquisition",
        ));
    }
    Ok(SocketLock { _file: file })
}

pub(super) fn ensure_owner(meta: &fs::Metadata, what: &str) -> io::Result<()> {
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{what} is not owned by effective uid"),
        ));
    }
    Ok(())
}

pub(super) fn ensure_entry_owner(meta: EntryMetadata, what: &str) -> io::Result<()> {
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    if meta.uid != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{what} is not owned by effective uid"),
        ));
    }
    Ok(())
}

pub(super) fn is_stale_connect_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ECONNREFUSED | libc::ENOENT | libc::ECONNRESET | libc::ENOTCONN)
    )
}
