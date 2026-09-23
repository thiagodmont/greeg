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
