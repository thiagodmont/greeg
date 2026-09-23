//! Private greeg-owned storage. One directory is opened and checked once;
//! every leaf operation then goes through its descriptor without following
//! symlinks, so a swapped path cannot redirect a write. Owned objects must
//! belong to this user, have the expected type and a single link, and carry
//! no extended ACL; their mode is tightened to 0700/0600 when needed.

use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// A single path component: never empty, `.`, `..` or containing `/`.
fn cname(name: impl AsRef<OsStr>) -> io::Result<CString> {
    let bytes = name.as_ref().as_bytes();
    if matches!(bytes, b"" | b"." | b"..") || bytes.contains(&b'/') {
        return Err(invalid("private storage names are single path components"));
    }
    CString::new(bytes).map_err(|_| invalid("invalid private storage name"))
}

fn open_at(dir: &File, name: impl AsRef<OsStr>, flags: i32) -> io::Result<File> {
    openat_c(dir, &cname(name)?, flags)
}

fn openat_c(dir: &File, name: &CStr, flags: i32) -> io::Result<File> {
    // SAFETY: the directory descriptor and NUL-terminated name remain valid.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Check an opened object and tighten its mode. With `repair` false it is
/// never chmodded: one that other users can access is refused, and one with
/// only owner bits (a sticky or setgid bit, say) is left as it is.
fn private(file: &File, directory: bool, repair: bool) -> io::Result<()> {
    let m = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if m.uid() != unsafe { libc::geteuid() }
        || if directory {
            !m.is_dir()
        } else {
            !m.is_file() || m.nlink() != 1
        }
    {
        return Err(invalid(
            "private object must be owned by this user and have the expected type/link count",
        ));
    }
    no_extended_acl(file)?;
    let mode = if directory { 0o700 } else { 0o600 };
    if !repair {
        if m.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "directory is accessible by other users; restrict it to its owner (chmod 700)",
            ));
        }
    } else if m.mode() & 0o7777 != mode {
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn no_extended_acl(file: &File) -> io::Result<()> {
    unsafe extern "C" {
        fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_int) -> *mut libc::c_void;
        fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
    }
    const ACL_TYPE_EXTENDED: libc::c_int = 0x100;
    // SAFETY: the descriptor is valid; the returned ACL is freed exactly once.
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if !acl.is_null() {
        unsafe {
            acl_free(acl);
        }
        return Err(invalid("extended ACLs on private storage are unsupported"));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(target_os = "linux")]
fn no_extended_acl(file: &File) -> io::Result<()> {
    for name in [c"system.posix_acl_access", c"system.posix_acl_default"] {
        // SAFETY: a zero-length query with a null buffer only returns the xattr size.
        let size =
            unsafe { libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0) };
        if size >= 0 {
            return Err(invalid("extended ACLs on private storage are unsupported"));
        }
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::ENODATA | libc::ENOTSUP)) {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn no_extended_acl(_: &File) -> io::Result<()> {
    Err(invalid("unsupported private storage platform"))
}

/// An advisory lock, released on drop (closing alone can leave one held by a
/// descriptor inherited across a fork).
pub struct Lock(File);

impl Lock {
    /// The locked descriptor (to model one inherited by a child process).
    pub fn file(&self) -> &File {
        &self.0
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: fdopendir transferred exclusive ownership to this stream.
        unsafe { libc::closedir(self.0) };
    }
}

/// A private directory, held open by descriptor.
pub struct PrivateDir {
    dir: File,
}

impl PrivateDir {
    /// Open `parent/name`, a directory greeg owns, creating it 0700 when
    /// missing and tightening it otherwise. Missing parents are created 0700;
    /// existing parents are never chmodded.
    pub fn open(parent: &Path, name: impl AsRef<OsStr>) -> io::Result<Self> {
        Self::open_with(parent, name.as_ref(), true)
    }

    /// `open`, but an existing directory the caller chose is only checked:
    /// one that other users can access is refused, never chmodded.
    pub fn open_chosen(parent: &Path, name: impl AsRef<OsStr>) -> io::Result<Self> {
        Self::open_with(parent, name.as_ref(), false)
    }

    fn open_with(parent: &Path, name: &OsStr, repair: bool) -> io::Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        let parent = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(parent)?;
        let c = cname(name)?;
        // SAFETY: valid directory descriptor and NUL-terminated name.
        if unsafe { libc::mkdirat(parent.as_raw_fd(), c.as_ptr(), 0o700) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        let dir = open_at(&parent, name, libc::O_RDONLY | libc::O_DIRECTORY)?;
        private(&dir, true, repair)?;
        Ok(Self { dir })
    }

    /// Open a checked private file. With `O_CREAT` (and no `O_EXCL`) an
    /// existing file is reused and a new one is created 0600. `O_TRUNC`
    /// applies only after the checks pass.
    pub fn file(&self, name: &str, flags: i32) -> io::Result<File> {
        let truncate = flags & libc::O_TRUNC != 0;
        let flags = flags & !libc::O_TRUNC;
        let file = if flags & libc::O_CREAT != 0 && flags & libc::O_EXCL == 0 {
            match open_at(&self.dir, name, flags | libc::O_EXCL) {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    open_at(&self.dir, name, flags & !libc::O_CREAT)?
                }
                Err(e) => return Err(e),
            }
        } else {
            open_at(&self.dir, name, flags)?
        };
        private(&file, false, true)?;
        if truncate {
            file.set_len(0)?;
        }
        Ok(file)
    }

    /// Open without the ownership checks (for inspection only).
    pub fn open_unchecked(&self, name: &str, flags: i32) -> io::Result<File> {
        open_at(&self.dir, name, flags)
    }

    /// Wait up to `wait` for an exclusive lock on the private file `name`.
    pub fn lock(&self, name: &str, wait: Duration) -> io::Result<Lock> {
        let lock = self
            .file(name, libc::O_RDWR | libc::O_CREAT)
            .map_err(|e| io::Error::new(e.kind(), format!("open lock: {e}")))?;
        let deadline = Instant::now() + wait;
        loop {
            match lock.try_lock() {
                Ok(()) => return Ok(Lock(lock)),
                Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Take the lock `name` only if it is free.
    pub fn try_lock(&self, name: &str) -> Option<Lock> {
        let lock = self.file(name, libc::O_RDWR | libc::O_CREAT).ok()?;
        lock.try_lock().ok()?;
        Some(Lock(lock))
    }

    pub fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let (from, to) = (cname(from)?, cname(to)?);
        // SAFETY: both names and the anchored directory descriptor are valid.
        if unsafe {
            libc::renameat(
                self.dir.as_raw_fd(),
                from.as_ptr(),
                self.dir.as_raw_fd(),
                to.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Remove a leaf; returns whether it existed.
    pub fn unlink(&self, name: &str) -> io::Result<bool> {
        let name = cname(name)?;
        // SAFETY: valid directory descriptor and NUL-terminated leaf name.
        if unsafe { libc::unlinkat(self.dir.as_raw_fd(), name.as_ptr(), 0) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(error)
        }
    }

    /// Atomically replace `name` with `body` through a private temporary file.
    pub fn replace(&self, name: &str, body: &[u8]) -> io::Result<()> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let mut attempts = 0;
        let (tmp, mut file) = loop {
            attempts += 1;
            if attempts > 128 {
                return Err(invalid("private temporary file collision limit"));
            }
            let tmp = format!(
                ".tmp-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            match self.file(&tmp, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL) {
                Ok(file) => break (tmp, file),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        };
        let result = file.write_all(body).and_then(|_| self.rename(&tmp, name));
        let _ = self.unlink(&tmp);
        result
    }

    /// Up to `limit` entry names, read from an independent directory stream.
    pub fn names(&self, limit: usize) -> Vec<String> {
        let mut out = Vec::new();
        // a new open description keeps each listing's offset independent
        let Ok(directory) = openat_c(&self.dir, c".", libc::O_RDONLY | libc::O_DIRECTORY) else {
            return out;
        };
        // SAFETY: directory owns a readable directory descriptor.
        let stream = unsafe { libc::fdopendir(directory.as_raw_fd()) };
        if stream.is_null() {
            return out;
        }
        let _ = directory.into_raw_fd();
        let stream = DirectoryStream(stream);
        for _ in 0..limit {
            // SAFETY: the stream is live and exclusively accessed here.
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                break;
            }
            // SAFETY: readdir supplies a NUL-terminated name, valid until the next call.
            if let Ok(name) = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_str() {
                out.push(name.to_owned());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own_dir() -> std::path::PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        loop {
            let d = std::env::temp_dir().join(format!(
                "greeg-private-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::DirBuilder::new().mode(0o700).create(&d) {
                Ok(()) => return d,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create {}: {e}", d.display()),
            }
        }
    }

    #[test]
    fn names_are_single_components() {
        let base = own_dir();
        let d = PrivateDir::open(&base, "store").unwrap();
        for bad in ["", ".", "..", "a/b", "../x"] {
            assert!(d.file(bad, libc::O_RDONLY).is_err(), "{bad:?}");
            assert!(d.open_unchecked(bad, libc::O_RDONLY).is_err(), "{bad:?}");
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn chosen_directories_are_never_chmodded() {
        let base = own_dir();
        let chosen = base.join("chosen");
        fs::create_dir(&chosen).unwrap();
        fs::set_permissions(&chosen, fs::Permissions::from_mode(0o1700)).unwrap();
        PrivateDir::open_chosen(&base, "chosen").unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().mode() & 0o7777;
        assert_eq!(mode(&chosen), 0o1700);
        fs::set_permissions(&chosen, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(PrivateDir::open_chosen(&base, "chosen").is_err());
        assert_eq!(mode(&chosen), 0o750);
        // a non-UTF-8 name is a valid directory name (APFS rejects the bytes)
        #[cfg(target_os = "linux")]
        {
            let odd = std::ffi::OsStr::from_bytes(b"st\xffts");
            PrivateDir::open(&base, odd).unwrap();
            assert!(base.join(odd).is_dir());
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn truncation_waits_for_the_checks() {
        let base = own_dir();
        let d = PrivateDir::open(&base, "store").unwrap();
        let outside = base.join("outside");
        fs::write(&outside, "keep").unwrap();
        fs::hard_link(&outside, base.join("store/linked")).unwrap();
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC;
        assert!(d.file("linked", flags).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        fs::write(base.join("store/own"), "old").unwrap();
        d.file("own", flags).unwrap();
        assert_eq!(fs::read_to_string(base.join("store/own")).unwrap(), "");
        let _ = fs::remove_dir_all(&base);
    }
}
