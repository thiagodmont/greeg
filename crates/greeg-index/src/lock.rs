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

/// A work marker (`BUILDING`, `REFRESHING`): its owner holds an exclusive
/// `flock` on it while the work runs, so it is live exactly as long as that
/// process. Dropping it removes the file, then releases the lock.
pub struct Marker {
    path: std::path::PathBuf,
    _file: File,
}

impl Drop for Marker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Take the marker `<dir>/<name>`, or `None` while a live process holds it.
/// One left by a process that died is taken over.
pub fn take_marker(dir: &Path, name: &str) -> Result<Option<Marker>> {
    use std::io::Write as _;
    use std::os::unix::fs::MetadataExt as _;
    crate::persist::check()?;
    crate::create_private_dir(dir)?;
    let path = dir.join(name);
    loop {
        let mut options = crate::private_file();
        options.read(true).write(true).create(true);
        let mut file = options
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        if !lock_soon(&file).with_context(|| format!("lock {}", path.display()))? {
            return Ok(None);
        }
        // the owner before us may have removed this file after we opened it
        let ours = file.metadata()?;
        let same =
            std::fs::metadata(&path).is_ok_and(|m| (m.dev(), m.ino()) == (ours.dev(), ours.ino()));
        if !same {
            continue;
        }
        crate::owner_only(&file)?;
        file.set_len(0)?;
        writeln!(file, "{}", std::process::id())?;
        return Ok(Some(Marker { path, _file: file }));
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

/// A live process holds the marker `<dir>/<name>`.
pub fn marker_held(dir: &Path, name: &str) -> bool {
    let Ok(file) = File::open(dir.join(name)) else {
        return false;
    };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}
