//! Block digests (ARCHITECTURE.md): every component body is split into
//! blocks, and a trailer after it holds an XXH3-64 digest per block, fast
//! enough that a query hashes what it reads at little cost. The manifest
//! records the blake3 digest of each trailer, so a reader trusts no
//! byte it has not checked: small or eagerly read components are verified
//! whole when opened, and the large ones block by block as they are read,
//! each block once per process. A failed check marks the index corrupt;
//! readers then answer from a scan. This detects accidental corruption, not
//! a same-user adversary.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

/// Block size of the components a build writes.
pub const BLOCK: usize = 16 << 10;
const DIGEST: usize = 8;

/// Trailer bytes for a body of `len` bytes in blocks of `block`.
pub fn trailer_len(len: u64, block: u32) -> u64 {
    len.div_ceil(u64::from(block.max(1))) * DIGEST as u64
}

fn digest(block: &[u8]) -> [u8; DIGEST] {
    xxhash_rust::xxh3::xxh3_64(block).to_le_bytes()
}

/// The trailer of `body`: one digest per block.
pub fn trailer(body: &[u8], block: usize) -> Vec<u8> {
    let mut t = Vec::with_capacity(body.len().div_ceil(block) * DIGEST);
    for b in body.chunks(block) {
        t.extend_from_slice(&digest(b));
    }
    t
}

/// The digest of a trailer, which the manifest records for its component.
pub fn root(trailer: &[u8]) -> String {
    blake3::hash(trailer).to_hex()[..32].to_string()
}

static FAILED: AtomicBool = AtomicBool::new(false);

/// Record a failed check made outside `Blocks` (a trailer that does not
/// match the manifest).
pub fn note_failed() {
    FAILED.store(true, Ordering::Relaxed);
}

/// A check failed in this process: an index answer may rest on bytes that
/// are not what the build wrote, so it must not be given.
pub fn failed() -> bool {
    FAILED.load(Ordering::Relaxed)
}

/// Which blocks of a component this process has verified, and whether one
/// failed.
struct Checked {
    done: Box<[AtomicU64]>,
    bad: AtomicBool,
}

/// A component file: its trailer's digest, device and inode.
pub type Key = (String, u64, u64);

/// What this process verified, by component file, so an index opened again
/// (a verb, then the scan it hands over to) does not hash the same blocks
/// again. Files are never rewritten in place.
static CHECKED: LazyLock<Mutex<HashMap<Key, Arc<Checked>>>> = LazyLock::new(Default::default);

/// Forget what this process verified: for a test that rewrites a component
/// in place between opens.
#[doc(hidden)]
pub fn forget_checked() {
    CHECKED.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// The blocks of one component body, and which of them this process has
/// verified.
pub struct Blocks {
    body: &'static [u8],
    trailer: &'static [u8],
    block: usize,
    checked: Arc<Checked>,
}

impl Blocks {
    /// `key` shares what is verified with other opens of the same file.
    pub fn new(
        body: &'static [u8],
        trailer: &'static [u8],
        block: usize,
        key: Option<Key>,
    ) -> Arc<Blocks> {
        let n = body.len().div_ceil(block.max(1));
        let fresh = || {
            Arc::new(Checked {
                done: (0..n.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(),
                bad: AtomicBool::new(false),
            })
        };
        let checked = match key {
            Some(k) => CHECKED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(k)
                .or_insert_with(fresh)
                .clone(),
            None => fresh(),
        };
        Arc::new(Blocks {
            body,
            trailer,
            block: block.max(1),
            checked,
        })
    }

    /// Some block failed its check.
    pub fn bad(&self) -> bool {
        self.checked.bad.load(Ordering::Relaxed)
    }

    fn fail(&self) -> bool {
        self.checked.bad.store(true, Ordering::Relaxed);
        FAILED.store(true, Ordering::Relaxed);
        false
    }

    /// Verify blocks `first..=last`.
    fn verify(&self, first: usize, last: usize) -> bool {
        for i in first..=last {
            let (word, bit) = (&self.checked.done[i / 64], 1u64 << (i % 64));
            if word.load(Ordering::Relaxed) & bit != 0 {
                continue;
            }
            let start = i * self.block;
            let block = &self.body[start..(start + self.block).min(self.body.len())];
            let want = &self.trailer[i * DIGEST..(i + 1) * DIGEST];
            if digest(block) != want {
                return self.fail();
            }
            word.fetch_or(bit, Ordering::Relaxed);
        }
        true
    }

    /// Verify the bytes `off..off + len` of the body.
    pub fn range(&self, off: usize, len: usize) -> bool {
        if self.bad() {
            return false;
        }
        if len == 0 {
            return true;
        }
        match off.checked_add(len) {
            Some(end) if end <= self.body.len() => {
                self.verify(off / self.block, (end - 1) / self.block)
            }
            _ => self.fail(),
        }
    }

    /// Verify the bytes of `s`, a slice of the body.
    pub fn slice<T>(&self, s: &[T]) -> bool {
        let start = s.as_ptr() as usize;
        let base = self.body.as_ptr() as usize;
        match start.checked_sub(base) {
            Some(off) => self.range(off, std::mem::size_of_val(s)),
            None => self.fail(),
        }
    }

    /// Verify the whole body.
    pub fn all(&self) -> bool {
        self.range(0, self.body.len())
    }
}

/// `Blocks::slice`, or true for a view that has no blocks to check (built
/// in memory, or verified whole when opened).
pub fn ok<T>(blocks: &Option<Arc<Blocks>>, s: &[T]) -> bool {
    blocks.as_ref().is_none_or(|b| b.slice(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leak(v: Vec<u8>) -> &'static [u8] {
        Box::leak(v.into_boxed_slice())
    }

    #[test]
    fn only_the_blocks_read_are_checked_and_a_flip_is_found() {
        let body: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let t = leak(trailer(&body, 1024));
        assert_eq!(t.len() as u64, trailer_len(body.len() as u64, 1024));
        let mut bad = body.clone();
        bad[5000] ^= 1;
        let b = Blocks::new(leak(bad), t, 1024, None);
        assert!(b.range(0, 4096), "blocks before the flip");
        assert!(b.range(9000, 1000), "the short last block");
        assert!(!b.bad());
        assert!(!b.range(4990, 20), "the flipped block");
        assert!(b.bad());
        // once bad, nothing is trusted
        assert!(!b.range(0, 10));
        assert!(!b.range(9000, 2000), "past the end");
    }

    #[test]
    fn another_open_of_the_same_file_reuses_what_was_verified() {
        let body: Vec<u8> = (0..4096u32).map(|i| (i % 13) as u8).collect();
        let t = leak(trailer(&body, 1024));
        let key = || Some((root(t), u64::MAX, 7));
        let first = Blocks::new(leak(body.clone()), t, 1024, key());
        assert!(first.range(0, 1024));
        // the same file mapped again: its first block is not hashed again,
        // so a change there, which a file never gets in place, goes unseen
        let mut changed = body.clone();
        changed[0] ^= 1;
        changed[2048] ^= 1;
        let again = Blocks::new(leak(changed.clone()), t, 1024, key());
        assert!(again.range(0, 1024));
        assert!(!again.range(2048, 1));
        assert!(first.bad(), "a failure is the file's, in every open");
        // another file with the same content shares nothing
        let other = Blocks::new(leak(changed), t, 1024, Some((root(t), u64::MAX, 8)));
        assert!(!other.range(0, 1024));
    }
}
