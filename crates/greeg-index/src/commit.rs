//! The point after which an answer cannot be taken back (ARCHITECTURE.md):
//! its first byte written to stdout. Every index snapshot is closed before
//! then, except in a streamed answer, so a failed check or a fault is
//! recovered from once, by answering again without the index; after it, a
//! fault ends the run with the output marked incomplete, never a second
//! answer.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static COMMITTED: AtomicBool = AtomicBool::new(false);
static STDIN_READ: AtomicBool = AtomicBool::new(false);
static STREAMING: AtomicBool = AtomicBool::new(false);

/// The answer is about to reach stdout.
pub fn commit() {
    COMMITTED.store(true, Ordering::Relaxed);
}

/// Some of the answer may have reached stdout. Async-signal-safe.
pub fn committed() -> bool {
    COMMITTED.load(Ordering::Relaxed)
}

/// This run consumed stdin, so it cannot be run again.
pub fn note_stdin_read() {
    STDIN_READ.store(true, Ordering::Relaxed);
}

/// Async-signal-safe.
pub fn stdin_read() -> bool {
    STDIN_READ.load(Ordering::Relaxed)
}

/// This answer is written as it is found (`--budget 0`): the index stays in
/// use after the first byte, and a failure after it ends the run with the
/// output marked incomplete.
pub fn stream() {
    STREAMING.store(true, Ordering::Relaxed);
}

/// The index is being used: in debug builds, a use after the output was
/// committed panics, unless the answer streams.
#[inline]
pub fn assert_open() {
    debug_assert!(
        !committed() || STREAMING.load(Ordering::Relaxed),
        "the index was used after output started"
    );
}

/// Fault injection for the recovery tests: `GREEG_DEBUG_SIGBUS` names where
/// to raise SIGBUS, `after-output`, `after-stdin` or `every-run`; any other
/// value raises it when an index is opened (`index`).
pub fn inject(point: &str) {
    static AT: OnceLock<Option<String>> = OnceLock::new();
    let Some(at) = AT.get_or_init(|| {
        std::env::var_os("GREEG_DEBUG_SIGBUS").map(|v| {
            let v = v.to_string_lossy().into_owned();
            match v.as_str() {
                "after-output" | "after-stdin" | "every-run" => v,
                _ => "index".to_string(),
            }
        })
    }) else {
        return;
    };
    if at == point {
        unsafe { libc::raise(libc::SIGBUS) };
    }
}
