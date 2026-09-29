//! Whether this process may write what greeg keeps on disk. `--no-persist`
//! turns it off: queries answer from what is there, and every write
//! primitive refuses, so a path that forgets to check fails instead of
//! writing.

use std::sync::atomic::{AtomicBool, Ordering};

static OFF: AtomicBool = AtomicBool::new(false);

pub fn disable() {
    OFF.store(true, Ordering::Relaxed);
}

pub fn allowed() -> bool {
    !OFF.load(Ordering::Relaxed)
}

/// `Ok` when writes are allowed.
pub fn check() -> std::io::Result<()> {
    if allowed() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "--no-persist: greeg writes nothing to disk",
        ))
    }
}
