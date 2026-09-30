//! What an index's symbols, spans and imports were derived with
//! (ARCHITECTURE.md): the version, the built-in tags queries, the parse
//! budget and every registered extra language. The manifest records it, and an index derived
//! otherwise is rebuilt; it also names each language code of the file table.

use greeg_lang::extra::{self, ExtraLang};
use greeg_lang::{BUILTINS, sym};
use std::os::unix::fs::MetadataExt;

/// What this binary derives with, whatever extra languages are registered:
/// its version, built-in tags queries and parse budget. It names the index
/// directory (`format_dir`), so builds that derive differently keep separate
/// indexes instead of rebuilding each other's.
pub fn build_key() -> &'static str {
    static K: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    K.get_or_init(|| key_of(env!("CARGO_PKG_VERSION"), &sym::QUERIES, sym::PARSE_BUDGET))
}

fn key_of(version: &str, queries: &[&str], parse_budget: u32) -> String {
    let mut h = blake3::Hasher::new();
    h.update(version.as_bytes());
    for q in queries {
        h.update(&(q.len() as u64).to_le_bytes());
        h.update(q.as_bytes());
    }
    h.update(&parse_budget.to_le_bytes());
    h.finalize().to_hex()[..16].to_string()
}

/// This process's derivation. Computed once: a few small reads and stats
/// per extra language.
pub fn derivation() -> &'static str {
    static D: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    D.get_or_init(|| {
        let mut h = blake3::Hasher::new();
        h.update(build_key().as_bytes());
        for f in fingerprints() {
            h.update(f.as_bytes());
        }
        h.finalize().to_hex()[..32].to_string()
    })
}

/// A name for every language code (`Lang::code` is its position), stable
/// across processes: `builtin:<name>`, or `extra:<name>@<fingerprint>`.
pub fn language_keys() -> Vec<String> {
    let mut keys: Vec<String> = BUILTINS
        .iter()
        .map(|l| format!("builtin:{}", l.name()))
        .collect();
    for (l, f) in extra::registry().iter().zip(fingerprints()) {
        keys.push(format!("extra:{}@{}", l.name, &f[..16]));
    }
    keys
}

/// `fingerprint` of each registered language, in registry order.
fn fingerprints() -> &'static [String] {
    static F: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    F.get_or_init(|| extra::registry().iter().map(fingerprint).collect())
}

/// What of a registered language decides its symbols: its name,
/// extensions, spec, tags query, and the grammar file's identity (device,
/// inode, size, mtime, ctime; the grammar is not loaded).
fn fingerprint(l: &ExtraLang) -> String {
    let mut h = blake3::Hasher::new();
    h.update(l.name.as_bytes());
    for e in &l.extensions {
        h.update(&[0]);
        h.update(e.as_bytes());
    }
    for name in ["spec.toml", "tags.scm"] {
        let body = std::fs::read(l.dir.join(name)).unwrap_or_default();
        h.update(&(body.len() as u64).to_le_bytes());
        h.update(&body);
    }
    for name in ["grammar.dylib", "grammar.so"] {
        match std::fs::metadata(l.dir.join(name)) {
            Ok(md) => {
                h.update(&[1]);
                for v in [md.dev(), md.ino(), md.size()] {
                    h.update(&v.to_le_bytes());
                }
                for v in [md.mtime(), md.mtime_nsec(), md.ctime(), md.ctime_nsec()] {
                    h.update(&v.to_le_bytes());
                }
            }
            Err(_) => {
                h.update(&[0]);
            }
        }
    }
    h.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two builds of one version whose parses stop at different points
    /// extract differently, so they keep separate indexes.
    #[test]
    fn the_parse_budget_is_part_of_the_build_key() {
        let key = |budget| key_of("1.0.0", &sym::QUERIES, budget);
        assert_eq!(key(sym::PARSE_BUDGET), key(sym::PARSE_BUDGET));
        assert_ne!(key(sym::PARSE_BUDGET), key(sym::PARSE_BUDGET / 2));
    }
}
