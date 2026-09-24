//! Root-relative paths: the bytes of each name below the root, joined by
//! `/`, the only separator on Unix. No byte is converted or replaced, so a
//! path names the same file whatever its name holds.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The path below `root` as bytes; `p` itself when it is not below it.
pub fn of(root: &Path, p: &Path) -> Vec<u8> {
    p.strip_prefix(root)
        .unwrap_or(p)
        .as_os_str()
        .as_bytes()
        .to_vec()
}

pub fn as_path(rel: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(rel))
}

/// Everything before the last `/`; empty at the root.
pub fn parent(rel: &[u8]) -> &[u8] {
    rel.iter()
        .rposition(|&b| b == b'/')
        .map_or(&[][..], |i| &rel[..i])
}

pub fn file_name(rel: &[u8]) -> &[u8] {
    rel.iter()
        .rposition(|&b| b == b'/')
        .map_or(rel, |i| &rel[i + 1..])
}

pub fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        return name.to_vec();
    }
    let mut out = Vec::with_capacity(dir.len() + 1 + name.len());
    out.extend_from_slice(dir);
    out.push(b'/');
    out.extend_from_slice(name);
    out
}

/// For text meant to be read rather than reopened: invalid UTF-8 and
/// control bytes become `\xNN`. Anything else is returned as it is. Path
/// heuristics (flags, language hints) read this form too, so the index and a
/// scan classify a name the same way.
pub fn display(rel: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(rel) {
        Ok(s) if !s.bytes().any(|b| b.is_ascii_control()) => Cow::Borrowed(s),
        _ => {
            let mut out = String::with_capacity(rel.len() + 8);
            for chunk in rel.utf8_chunks() {
                for c in chunk.valid().chars() {
                    if c.is_ascii_control() {
                        out.push_str(&format!("\\x{:02X}", c as u8));
                    } else {
                        out.push(c);
                    }
                }
                for b in chunk.invalid() {
                    out.push_str(&format!("\\x{b:02X}"));
                }
            }
            Cow::Owned(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_split_only_at_slashes() {
        assert_eq!(parent(b"a\\b/c\\d.txt"), b"a\\b");
        assert_eq!(file_name(b"a\\b/c\\d.txt"), b"c\\d.txt");
        assert_eq!(parent(b"x\xff.rs"), b"");
        assert_eq!(join(b"", b"a"), b"a");
        assert_eq!(join(b"d\xfe", b"a"), b"d\xfe/a");
        assert_eq!(of(Path::new("/r"), Path::new("/r/a\\b.txt")), b"a\\b.txt");
    }

    #[test]
    fn display_escapes_only_what_cannot_be_read() {
        assert!(matches!(
            display(b"src/a\\b.rs"),
            Cow::Borrowed("src/a\\b.rs")
        ));
        assert_eq!(display(b"caf\xc3\xa9/x\xff\ty.rs"), "café/x\\xFF\\x09y.rs");
    }
}
