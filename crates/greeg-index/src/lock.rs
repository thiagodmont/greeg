//! Writer lock (ARCHITECTURE.md): every writer (build publish, delta apply)
//! holds an exclusive `flock` on `<index dir>/LOCK` while it reads the
//! manifest, writes components and republishes. Readers never lock.

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::path::Path;

/// Held for the lifetime of the value; dropping it releases the lock.
pub struct WriterLock {
    _file: File,
}

/// Block until the exclusive writer lock for `dir` is held.
pub fn writer(dir: &Path) -> Result<WriterLock> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("LOCK");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    file.lock()
        .with_context(|| format!("lock {}", path.display()))?;
    Ok(WriterLock { _file: file })
}

/// Take the writer lock without blocking; `None` when another writer holds it.
pub fn try_writer(dir: &Path) -> Result<Option<WriterLock>> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("LOCK");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(WriterLock { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("lock {}", path.display()))
        }
    }
}
