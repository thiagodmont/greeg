//! Reading below a directory without following a symlink in any component
//! under it (ARCHITECTURE.md): a directory swapped for a symlink cannot
//! redirect a read out of the tree. The directory itself is opened once,
//! following symlinks in its own path.
//!
//! macOS 11+ refuses symlinks in one call (`O_NOFOLLOW_ANY`,
//! `AT_SYMLINK_NOFOLLOW_ANY`) and Linux 5.6+ opens with `openat2`; otherwise
//! each directory on the way is opened with `O_NOFOLLOW`, and a stat pass
//! keeps the directories of the previous path ([`Dirs`]).

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// A directory files are read below.
#[derive(Debug)]
pub struct Tree {
    dir: File,
    /// Where lookups start: `AT_FDCWD` when the directory is the working
    /// directory, which the process holds as it holds `dir` and which
    /// macOS resolves faster from many threads; `dir` otherwise.
    at: RawFd,
    path: PathBuf,
}

/// What `lstat` says about an entry.
pub type Stat = libc::stat;

pub fn is_file(st: &Stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFREG
}

pub fn is_dir(st: &Stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not a path below the tree")
}

/// `rel` as a C path: relative, with no empty, `.` or `..` component.
fn cpath(rel: &[u8]) -> io::Result<CString> {
    if rel
        .split(|&b| b == b'/')
        .any(|c| matches!(c, b"" | b"." | b".."))
    {
        return Err(invalid());
    }
    CString::new(rel).map_err(|_| invalid())
}

fn openat(dir: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    // SAFETY: a valid descriptor and NUL-terminated name.
    let fd = unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn fstatat(dir: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<Stat> {
    // SAFETY: a valid descriptor, NUL-terminated name and stat buffer.
    let mut st: Stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

/// Directories on the way are opened only to look up names below them.
#[cfg(target_os = "linux")]
const DIR_FLAGS: libc::c_int = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW;
#[cfg(not(target_os = "linux"))]
const DIR_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;

const LEAF_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK;

#[cfg(target_os = "macos")]
mod fast {
    use super::*;
    use std::sync::OnceLock;

    /// `sys/fcntl.h`, macOS 11+.
    const AT_SYMLINK_NOFOLLOW_ANY: libc::c_int = 0x0800;

    /// Both flags arrived in macOS 11; an older kernel would ignore the open
    /// flag, so neither is used unless the stat flag is understood.
    fn available(dir: RawFd) -> bool {
        static OK: OnceLock<bool> = OnceLock::new();
        *OK.get_or_init(|| fstatat(dir, c".", AT_SYMLINK_NOFOLLOW_ANY).is_ok())
    }

    pub(super) fn open(dir: RawFd, rel: &CStr) -> Option<io::Result<File>> {
        available(dir).then(|| {
            openat(
                dir,
                rel,
                libc::O_RDONLY | libc::O_NOFOLLOW_ANY | libc::O_NONBLOCK,
            )
        })
    }

    pub(super) fn stat(dir: RawFd, rel: &CStr) -> Option<io::Result<Stat>> {
        available(dir).then(|| fstatat(dir, rel, AT_SYMLINK_NOFOLLOW_ANY))
    }
}

#[cfg(target_os = "linux")]
mod fast {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    static MISSING: AtomicBool = AtomicBool::new(false);

    /// `openat2` beneath `dir`, refusing any symlink; `None` when the kernel
    /// or a sandbox does not offer it, or a concurrent rename asks for a
    /// retry.
    pub(super) fn open(dir: RawFd, rel: &CStr) -> Option<io::Result<File>> {
        if MISSING.load(Ordering::Relaxed) {
            return None;
        }
        // SAFETY: open_how is plain data.
        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags = (LEAF_FLAGS | libc::O_CLOEXEC) as u64;
        how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS;
        // SAFETY: a valid descriptor, NUL-terminated name and open_how.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                dir,
                rel.as_ptr(),
                &how as *const libc::open_how,
                std::mem::size_of::<libc::open_how>(),
            )
        };
        if fd >= 0 {
            // SAFETY: openat2 returned a new owned descriptor.
            return Some(Ok(unsafe { File::from_raw_fd(fd as RawFd) }));
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ENOSYS | libc::EPERM) => {
                MISSING.store(true, Ordering::Relaxed);
                None
            }
            Some(libc::EAGAIN) => None,
            _ => Some(Err(e)),
        }
    }

    pub(super) fn stat(_: RawFd, _: &CStr) -> Option<io::Result<Stat>> {
        None
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod fast {
    use super::*;

    pub(super) fn open(_: RawFd, _: &CStr) -> Option<io::Result<File>> {
        None
    }

    pub(super) fn stat(_: RawFd, _: &CStr) -> Option<io::Result<Stat>> {
        None
    }
}

impl Tree {
    /// The directory at `root`, symlinks in its own path followed.
    pub fn open(root: &Path) -> io::Result<Tree> {
        let dir = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(root)?;
        let at = if same_file(&dir, Path::new(".")) {
            libc::AT_FDCWD
        } else {
            dir.as_raw_fd()
        };
        Ok(Tree {
            dir,
            at,
            path: root.to_path_buf(),
        })
    }

    /// The path the tree was opened at.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open the regular file at `rel` for reading, without blocking on a
    /// FIFO or device.
    pub fn open_file(&self, rel: &[u8]) -> io::Result<File> {
        let c = cpath(rel)?;
        let f = match fast::open(self.at, &c) {
            Some(r) => r?,
            None => self.walk_open(rel)?,
        };
        if !f.metadata()?.is_file() {
            return Err(io::Error::other("not a regular file"));
        }
        Ok(f)
    }

    /// The bytes of the regular file at `rel`.
    pub fn read(&self, rel: &[u8]) -> io::Result<Vec<u8>> {
        let mut v = Vec::new();
        self.open_file(rel)?.read_to_end(&mut v)?;
        Ok(v)
    }

    /// `lstat` of `rel` (the tree itself when empty), refusing a symlink in
    /// any directory on the way. `dirs` keeps the directories of the
    /// previous call, so stats in path order open each directory once.
    pub fn stat(&self, rel: &[u8], dirs: &mut Dirs) -> io::Result<Stat> {
        if rel.is_empty() {
            let mut st: Stat = unsafe { std::mem::zeroed() };
            // SAFETY: a valid descriptor and stat buffer.
            if unsafe { libc::fstat(self.dir.as_raw_fd(), &mut st) } != 0 {
                return Err(io::Error::last_os_error());
            }
            return Ok(st);
        }
        let c = cpath(rel)?;
        if let Some(r) = fast::stat(self.at, &c) {
            return r;
        }
        let (parent, leaf) = split(rel);
        let dir = dirs.enter(self, parent)?;
        fstatat(dir, &cpath(leaf)?, libc::AT_SYMLINK_NOFOLLOW)
    }

    /// Open `rel` one component at a time, none of them followed.
    fn walk_open(&self, rel: &[u8]) -> io::Result<File> {
        let (parent, leaf) = split(rel);
        let mut at: Option<File> = None;
        if !parent.is_empty() {
            for c in parent.split(|&b| b == b'/') {
                let base = at.as_ref().map_or(self.at, File::as_raw_fd);
                at = Some(openat(base, &cpath(c)?, DIR_FLAGS)?);
            }
        }
        let base = at.as_ref().map_or(self.at, File::as_raw_fd);
        openat(base, &cpath(leaf)?, LEAF_FLAGS)
    }
}

/// `f` is the file at `path`, by device and inode.
fn same_file(f: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (f.metadata(), std::fs::metadata(path)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// `rel` split at its last `/`.
fn split(rel: &[u8]) -> (&[u8], &[u8]) {
    match rel.iter().rposition(|&b| b == b'/') {
        Some(i) => (&rel[..i], &rel[i + 1..]),
        None => (&[], rel),
    }
}

/// The directories of the last path a stat went through, each opened
/// without following a symlink.
#[derive(Default)]
pub struct Dirs {
    open: Vec<(Vec<u8>, File)>,
}

impl Dirs {
    /// The descriptor of `parent` below `tree`, reusing the directories it
    /// shares with the previous one.
    fn enter(&mut self, tree: &Tree, parent: &[u8]) -> io::Result<RawFd> {
        let comps: Vec<&[u8]> = if parent.is_empty() {
            Vec::new()
        } else {
            parent.split(|&b| b == b'/').collect()
        };
        let keep = self
            .open
            .iter()
            .zip(&comps)
            .take_while(|((name, _), c)| name.as_slice() == **c)
            .count();
        self.open.truncate(keep);
        for c in &comps[keep..] {
            let base = self.open.last().map_or(tree.at, |(_, f)| f.as_raw_fd());
            match openat(base, &cpath(c)?, DIR_FLAGS) {
                Ok(f) => self.open.push((c.to_vec(), f)),
                Err(e) => {
                    self.open.clear();
                    return Err(e);
                }
            }
        }
        Ok(self.open.last().map_or(tree.at, |(_, f)| f.as_raw_fd()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn fixture(name: &str) -> std::path::PathBuf {
        // created here, so no file left by another run joins the fixture
        let d = (0..)
            .map(|n| {
                std::env::temp_dir().join(format!("greeg-tree-{name}-{}-{n}", std::process::id()))
            })
            .find(|d| match fs::create_dir(d) {
                Ok(()) => true,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
                Err(e) => panic!("create {}: {e}", d.display()),
            })
            .unwrap();
        fs::create_dir_all(d.join("root/src/deep")).unwrap();
        fs::create_dir_all(d.join("outside")).unwrap();
        fs::write(d.join("root/src/a.rs"), "inside\n").unwrap();
        fs::write(d.join("root/src/deep/b.rs"), "inside\n").unwrap();
        fs::write(d.join("outside/a.rs"), "outside\n").unwrap();
        fs::write(d.join("outside/c.rs"), "outside\n").unwrap();
        d
    }

    #[test]
    fn a_symlink_anywhere_below_the_tree_is_refused_on_every_path() {
        let d = fixture("refused");
        symlink(d.join("outside"), d.join("root/linked")).unwrap();
        symlink(d.join("outside/c.rs"), d.join("root/src/leaf.rs")).unwrap();
        // the tree itself may be reached through a symlink
        symlink(d.join("root"), d.join("via")).unwrap();
        let t = Tree::open(&d.join("via")).unwrap();
        assert_eq!(t.read(b"src/a.rs").unwrap(), b"inside\n");
        assert_eq!(t.read(b"src/deep/b.rs").unwrap(), b"inside\n");
        let mut dirs = Dirs::default();
        assert!(is_file(&t.stat(b"src/deep/b.rs", &mut dirs).unwrap()));
        assert!(is_dir(&t.stat(b"src", &mut dirs).unwrap()));
        assert!(is_dir(&t.stat(b"", &mut dirs).unwrap()));
        for rel in [&b"linked/a.rs"[..], b"src/leaf.rs", b"../outside/a.rs"] {
            for open in [Tree::open_file, Tree::walk_open] {
                let r = open(&t, rel);
                assert!(r.is_err(), "{}", String::from_utf8_lossy(rel));
            }
        }
        assert!(t.open_file(b"src").is_err(), "a directory is not a file");
        assert!(t.stat(b"linked/a.rs", &mut dirs).is_err());
        assert!(t.stat(b"../outside/a.rs", &mut dirs).is_err());
        // a symlink as the entry itself is reported, not followed
        let leaf = t.stat(b"src/leaf.rs", &mut dirs).unwrap();
        assert!(!is_file(&leaf) && !is_dir(&leaf));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_directory_swapped_after_the_tree_was_opened_cannot_redirect_reads() {
        let d = fixture("swapped");
        let t = Tree::open(&d.join("root")).unwrap();
        let mut dirs = Dirs::default();
        assert!(t.stat(b"src/a.rs", &mut dirs).is_ok());
        fs::rename(d.join("root/src"), d.join("moved")).unwrap();
        symlink(d.join("outside"), d.join("root/src")).unwrap();
        assert!(t.open_file(b"src/a.rs").is_err());
        assert!(t.walk_open(b"src/a.rs").is_err());
        // a fresh Dirs sees the swap; a stale one still holds the old
        // directory, whose files are the tree's own
        assert!(t.stat(b"src/a.rs", &mut Dirs::default()).is_err());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn stats_in_path_order_keep_the_shared_directories() {
        let d = fixture("order");
        fs::create_dir_all(d.join("root/src/deep/er")).unwrap();
        fs::write(d.join("root/src/deep/er/x.rs"), "").unwrap();
        fs::write(d.join("root/top.rs"), "").unwrap();
        let t = Tree::open(&d.join("root")).unwrap();
        let mut dirs = Dirs::default();
        for rel in [
            &b"src/a.rs"[..],
            b"src/deep/b.rs",
            b"src/deep/er/x.rs",
            b"src/a.rs",
            b"top.rs",
        ] {
            let parent = split(rel).0;
            let fd = dirs.enter(&t, parent).unwrap();
            let st = fstatat(fd, &cpath(split(rel).1).unwrap(), libc::AT_SYMLINK_NOFOLLOW).unwrap();
            assert!(is_file(&st), "{}", String::from_utf8_lossy(rel));
            let depth = if parent.is_empty() {
                0
            } else {
                parent.split(|&b| b == b'/').count()
            };
            assert_eq!(dirs.open.len(), depth);
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let d = fixture("fifo");
        let c = CString::new(d.join("root/src/pipe").as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let t = Tree::open(&d.join("root")).unwrap();
        assert!(t.open_file(b"src/pipe").is_err());
        let _ = fs::remove_dir_all(&d);
    }
}
