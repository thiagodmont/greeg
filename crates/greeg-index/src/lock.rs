//! Writer lock (ARCHITECTURE.md): every writer (build publish, delta apply)
//! holds an exclusive `flock` on `<index dir>/LOCK` while it reads the
//! manifest, writes components and republishes. Readers never lock.

use anyhow::{Context, Result};
use std::fs::File;
use std::path::Path;

/// Held for the lifetime of the value; dropping it releases the lock.
pub struct WriterLock {
    _file: File,
}

/// `<dir>/LOCK`, created owner-only when missing (an existing one is reused).
fn open_lock(dir: &Path) -> Result<(std::path::PathBuf, File)> {
    crate::persist::check()?;
    crate::legacy::secure(crate::repo_of(dir));
    crate::create_private_dir(dir)?;
    let path = dir.join("LOCK");
    let mut options = crate::private_file();
    options.read(true).write(true);
    let file = match options.clone().create_new(true).open(&path) {
        Ok(file) => {
            crate::owner_only(&file)?;
            file
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => options.open(&path)?,
        Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
    };
    Ok((path, file))
}

/// Block until the exclusive writer lock for `dir` is held.
pub fn writer(dir: &Path) -> Result<WriterLock> {
    let (path, file) = open_lock(dir)?;
    file.lock()
        .with_context(|| format!("lock {}", path.display()))?;
    Ok(WriterLock { _file: file })
}

/// Take the writer lock without blocking; `None` when another writer holds it.
pub fn try_writer(dir: &Path) -> Result<Option<WriterLock>> {
    let (path, file) = open_lock(dir)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(WriterLock { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("lock {}", path.display()))
        }
    }
}

/// The work a marker stands for, one process at a time per index.
#[derive(Clone, Copy)]
pub enum Work {
    Build,
    Refresh,
}

impl Work {
    pub fn name(self) -> &'static str {
        match self {
            Work::Build => "BUILDING",
            Work::Refresh => "REFRESHING",
        }
    }

    /// How long an older greeg's unlocked marker counts as live.
    fn legacy_secs(self) -> u64 {
        match self {
            Work::Build => 600,
            Work::Refresh => 30,
        }
    }
}

/// What a locked marker holds; an older greeg wrote only its pid.
const MARK: &[u8] = b"flock ";

/// A work marker: its owner holds an exclusive `flock` on it while the work
/// runs, so it is live exactly as long as that process. Dropping it removes
/// the file, if it is still this one, then releases the lock.
pub struct Marker {
    dir: crate::private::PrivateDir,
    name: &'static str,
    file: File,
}

impl Drop for Marker {
    fn drop(&mut self) {
        if same_file(&self.dir, self.name, &self.file) {
            let _ = self.dir.unlink(self.name);
        }
    }
}

/// `name` in `dir` is still the file `file` has open.
fn same_file(dir: &crate::private::PrivateDir, name: &str, file: &File) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let (Ok(now), Ok(ours)) = (
        dir.open_unchecked(name, libc::O_RDONLY)
            .and_then(|f| f.metadata()),
        file.metadata(),
    ) else {
        return false;
    };
    (now.dev(), now.ino()) == (ours.dev(), ours.ino())
}

/// An unlocked marker an older greeg wrote, young enough to be live.
fn legacy_live(file: &File, work: Work) -> bool {
    use std::io::Read as _;
    let mut head = [0u8; MARK.len()];
    let n = (&*file).read(&mut head).unwrap_or(0);
    let young = file
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.elapsed().ok())
        .is_none_or(|e| e.as_secs() < work.legacy_secs());
    n > 0 && head[..n] != MARK[..n] && young
}

/// Take the marker for `work` in `dir`, or `None` while a live process holds
/// it. One left by a process that died is taken over.
pub fn take_marker(dir: &Path, work: Work) -> Result<Option<Marker>> {
    use std::io::{Seek as _, Write as _};
    let (Some(parent), Some(leaf)) = (dir.parent(), dir.file_name()) else {
        anyhow::bail!("no index directory in {}", dir.display());
    };
    let dir = crate::private::PrivateDir::open(parent, leaf)
        .with_context(|| format!("open {}", dir.display()))?;
    let name = work.name();
    loop {
        // owned, regular, one link, not followed through a symlink
        let mut file = dir
            .file(name, libc::O_RDWR | libc::O_CREAT)
            .with_context(|| format!("open marker {name}"))?;
        if !lock_soon(&file).with_context(|| format!("lock marker {name}"))? {
            return Ok(None);
        }
        // the owner before us may have removed this file after we opened it
        if !same_file(&dir, name, &file) {
            continue;
        }
        if legacy_live(&file, work) {
            return Ok(None);
        }
        file.set_len(0)?;
        file.rewind()?;
        file.write_all(MARK)?;
        writeln!(file, "{}", std::process::id())?;
        return Ok(Some(Marker { dir, name, file }));
    }
}

/// The exclusive lock, unless another process keeps it: a [`marker_held`]
/// probe holds it only for an instant, so a few tries tell the two apart.
fn lock_soon(file: &File) -> std::io::Result<bool> {
    for _ in 0..5 {
        match file.try_lock() {
            Ok(()) => return Ok(true),
            Err(std::fs::TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
    Ok(false)
}

/// A live process holds the marker for `work` in `dir`. A lock that cannot
/// be probed reads as free: the process that takes it reports the error.
pub fn marker_held(dir: &Path, work: Work) -> bool {
    use std::os::unix::fs::OpenOptionsExt as _;
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join(work.name()))
    else {
        return false;
    };
    match file.try_lock() {
        Err(std::fs::TryLockError::WouldBlock) => true,
        Ok(()) => legacy_live(&file, work),
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};

    fn dir(name: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("greeg-marker-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let dir = base.join("index");
        crate::create_private_dir(&dir).unwrap();
        dir
    }

    fn age(path: &Path, secs: u64) {
        let f = fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn an_older_greegs_marker_is_live_for_its_old_window() {
        let d = dir("legacy");
        let path = d.join("BUILDING");
        fs::write(&path, "4242\n").unwrap();
        assert!(marker_held(&d, Work::Build));
        assert!(take_marker(&d, Work::Build).unwrap().is_none());
        age(&path, 601);
        assert!(!marker_held(&d, Work::Build));
        let m = take_marker(&d, Work::Build)
            .unwrap()
            .expect("a stale one is taken");
        assert!(fs::read(&path).unwrap().starts_with(MARK));
        assert!(marker_held(&d, Work::Build));
        drop(m);
        assert!(!path.exists());
        // a refresh's old window is thirty seconds
        let path = d.join("REFRESHING");
        fs::write(&path, "4242\n").unwrap();
        assert!(take_marker(&d, Work::Refresh).unwrap().is_none());
        age(&path, 31);
        assert!(take_marker(&d, Work::Refresh).unwrap().is_some());
        let _ = fs::remove_dir_all(d.parent().unwrap());
    }

    #[test]
    fn a_marker_reached_through_a_link_is_refused() {
        let d = dir("link");
        let outside = d.parent().unwrap().join("outside");
        fs::write(&outside, "keep").unwrap();
        std::os::unix::fs::symlink(&outside, d.join("BUILDING")).unwrap();
        assert!(take_marker(&d, Work::Build).is_err());
        fs::remove_file(d.join("BUILDING")).unwrap();
        fs::hard_link(&outside, d.join("BUILDING")).unwrap();
        assert!(take_marker(&d, Work::Build).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
        let _ = fs::remove_dir_all(d.parent().unwrap());
    }

    #[test]
    fn a_marker_removed_and_made_again_is_not_the_old_owners_to_remove() {
        let d = dir("replaced");
        let path = d.join("BUILDING");
        let m = take_marker(&d, Work::Build).unwrap().unwrap();
        // an older greeg removed it as stale, and another process made a new one
        fs::remove_file(&path).unwrap();
        fs::write(&path, "flock 7\n").unwrap();
        drop(m);
        assert_eq!(fs::read(&path).unwrap(), b"flock 7\n");
        let _ = fs::remove_dir_all(d.parent().unwrap());
    }
}
