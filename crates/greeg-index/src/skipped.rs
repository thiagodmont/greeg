//! What the index leaves out inside the directories it walks: hidden files
//! and directories, and entries an ignore rule excludes. A request can still
//! select some of them (ripgrep lets a positive glob select a hidden or an
//! ignored file, and a type select a hidden one), so a query checks this
//! record before trusting the index to cover the request.
//!
//! skipped.1.bin (build) and d-NNNN.skipped (a refresh that changed it), in
//! the build's directory; the manifest names the current one.
//!   u32 n, then n × (u8 bits, u32 len, path bytes), sorted by path

use crate::format::{self, is_ignore_file};
use crate::rel::join;
use anyhow::{Context, Result, bail};
use hashbrown::HashSet;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// A directory (otherwise a regular file).
pub const DIR: u8 = 1;
/// Excluded by an ignore rule (otherwise only for its hidden name, or a
/// tracked-only ignore file).
pub const IGNORED: u8 = 2;
/// The directory's children could not be listed (a read error): the entry
/// stands for the directory itself, and only a scan covers it.
pub const UNKNOWN: u8 = 4;

/// One directory's skipped children: (name bytes, bits).
pub type Children = Vec<(Vec<u8>, u8)>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Skipped {
    /// Walked directory → its skipped children.
    by_dir: BTreeMap<Vec<u8>, Children>,
}

impl Skipped {
    /// Replace one walked directory's skipped children.
    pub fn set(&mut self, dir: &[u8], mut kids: Children) {
        kids.sort();
        if kids.is_empty() {
            self.by_dir.remove(dir);
        } else {
            self.by_dir.insert(dir.to_vec(), kids);
        }
    }

    pub fn update(&mut self, dirs: &[(Vec<u8>, Children)]) {
        for (d, kids) in dirs {
            self.set(d, kids.clone());
        }
    }

    /// Every skipped entry as (root-relative path, bits); an `UNKNOWN`
    /// entry's path is its directory.
    pub fn entries(&self) -> impl Iterator<Item = (Vec<u8>, u8)> + '_ {
        self.by_dir.iter().flat_map(|(d, kids)| {
            kids.iter().map(move |(n, b)| {
                if n.is_empty() {
                    (d.clone(), *b)
                } else {
                    (join(d, n), *b)
                }
            })
        })
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let n: usize = self.by_dir.values().map(Vec::len).sum();
        out.extend_from_slice(&(n as u32).to_le_bytes());
        for (d, kids) in &self.by_dir {
            for (name, bits) in kids {
                let rel = join(d, name);
                out.push(*bits);
                out.extend_from_slice(&(rel.len() as u32).to_le_bytes());
                out.extend_from_slice(&rel);
            }
        }
        out
    }

    pub fn parse(body: &[u8]) -> Result<Skipped> {
        let mut s = Skipped::default();
        let mut at = 4;
        let n = u32::from_le_bytes(body.get(..4).context("skipped: no count")?.try_into()?);
        for _ in 0..n {
            let bits = *body.get(at).context("skipped: truncated")?;
            let len = u32::from_le_bytes(
                body.get(at + 1..at + 5)
                    .context("skipped: truncated")?
                    .try_into()?,
            ) as usize;
            let rel = body
                .get(at + 5..at + 5 + len)
                .context("skipped: truncated")?;
            at += 5 + len;
            s.by_dir
                .entry(crate::rel::parent(rel).to_vec())
                .or_default()
                .push((crate::rel::file_name(rel).to_vec(), bits));
        }
        if at != body.len() {
            bail!("skipped: trailing bytes");
        }
        Ok(s)
    }

    pub fn write(&self, path: &Path, id: format::Ident) -> Result<()> {
        format::write_atomic_with(path, format::COMP_SKIPPED, id, &self.serialize(), false)
    }

    /// A record file's contents; `None` when it is not a valid record.
    pub fn from_file_bytes(bytes: &[u8]) -> Option<Skipped> {
        Skipped::parse(format::check_header(bytes, format::COMP_SKIPPED).ok()?).ok()
    }
}

/// Where the record a manifest names lives. `None` when it names none
/// (built before the record existed): coverage is then unknown.
pub fn path(dir: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.starts_with('/') || name.split('/').any(|c| c == "..") {
        return None;
    }
    Some(dir.join(name))
}

/// The skipped children of each walked directory in `dirs`: every regular
/// file or directory in it that the walk did not keep. `kept` holds the
/// root-relative paths the walk yielded; `hidden` those it saw and left out
/// for their name. Anything else was excluded by an ignore rule before the
/// walk's filter saw it. Ignore files are yielded but never searched, so
/// they count as skipped hidden files. Lists on a few threads.
pub fn list(
    root: &Path,
    dirs: &[&[u8]],
    kept: &HashSet<&[u8]>,
    hidden: &HashSet<&[u8]>,
) -> Vec<(Vec<u8>, Children)> {
    const THREADS: usize = 4;
    let chunk = dirs.len().div_ceil(THREADS).max(1);
    std::thread::scope(|sc| {
        let parts: Vec<_> = dirs
            .chunks(chunk)
            .map(|part| {
                sc.spawn(move || {
                    part.iter()
                        .map(|d| (d.to_vec(), children(root, d, kept, hidden)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        parts
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

fn children(root: &Path, d: &[u8], kept: &HashSet<&[u8]>, hidden: &HashSet<&[u8]>) -> Children {
    let unknown = || vec![(Vec::new(), UNKNOWN)];
    let Ok(rd) = fs::read_dir(if d.is_empty() {
        root.to_path_buf()
    } else {
        root.join(crate::rel::as_path(d))
    }) else {
        return unknown();
    };
    let mut kids = Vec::new();
    for e in rd {
        let Ok(e) = e else { return unknown() };
        let Ok(ft) = e.file_type() else { continue };
        if !ft.is_dir() && !ft.is_file() {
            continue;
        }
        let name = e.file_name().as_bytes().to_vec();
        let rel = join(d, &name);
        let yielded = kept.contains(rel.as_slice());
        if yielded && !(ft.is_file() && is_ignore_file(&name)) {
            continue;
        }
        let mut bits = if ft.is_dir() { DIR } else { 0 };
        if !yielded && !hidden.contains(rel.as_slice()) {
            bits |= IGNORED;
        }
        kids.push((name, bits));
    }
    kids.sort();
    kids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_replace_per_directory() {
        let mut s = Skipped::default();
        s.set(
            b"",
            vec![(b".env".to_vec(), 0), (b"target".to_vec(), DIR | IGNORED)],
        );
        s.set(b"src", vec![(b"gen.rs".to_vec(), IGNORED)]);
        let back = Skipped::parse(&s.serialize()).unwrap();
        assert_eq!(back, s);
        assert_eq!(
            back.entries().collect::<Vec<_>>(),
            [
                (b".env".to_vec(), 0),
                (b"target".to_vec(), DIR | IGNORED),
                (b"src/gen.rs".to_vec(), IGNORED)
            ]
        );
        let mut s2 = back;
        s2.update(&[
            (b"src".to_vec(), vec![]),
            (b"".to_vec(), vec![(b".x".to_vec(), 0)]),
        ]);
        assert_eq!(s2.entries().collect::<Vec<_>>(), [(b".x".to_vec(), 0)]);
        assert!(Skipped::parse(&[1, 0, 0, 0]).is_err());
        assert!(path(Path::new("/"), "../x").is_none());
    }

    #[test]
    fn nested_unknown_markers_round_trip_and_a_refresh_replaces_them() {
        let mut s = Skipped::default();
        s.set(b"a/b", vec![(Vec::new(), UNKNOWN)]);
        let mut back = Skipped::parse(&s.serialize()).unwrap();
        assert_eq!(back, s);
        assert_eq!(
            back.entries().collect::<Vec<_>>(),
            [(b"a/b".to_vec(), UNKNOWN)]
        );
        back.update(&[(b"a/b".to_vec(), vec![(b".x".to_vec(), 0)])]);
        assert_eq!(
            back.entries().collect::<Vec<_>>(),
            [(b"a/b/.x".to_vec(), 0)]
        );
    }

    #[test]
    fn a_directory_that_cannot_be_listed_is_unknown() {
        let kept = HashSet::new();
        let got = list(
            Path::new("/nonexistent-greeg-root"),
            &[b"", b"a"],
            &kept,
            &kept,
        );
        assert_eq!(
            got,
            [
                (b"".to_vec(), vec![(Vec::new(), UNKNOWN)]),
                (b"a".to_vec(), vec![(Vec::new(), UNKNOWN)])
            ]
        );
        let mut s = Skipped::default();
        s.update(&got);
        let back = Skipped::parse(&s.serialize()).unwrap();
        assert_eq!(
            back.entries().collect::<Vec<_>>(),
            [(b"".to_vec(), UNKNOWN), (b"a".to_vec(), UNKNOWN)]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_name_that_is_not_utf8_is_recorded_as_it_is() {
        let base = std::env::temp_dir().join(format!("greeg-skipped-utf8-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join(std::ffi::OsStr::from_bytes(b".h\xff.rs")), "x").unwrap();
        let kept = HashSet::new();
        let got = list(&base, &[b""], &kept, &kept);
        assert_eq!(
            got,
            [(b"".to_vec(), vec![(b".h\xff.rs".to_vec(), IGNORED)])]
        );
        let mut s = Skipped::default();
        s.update(&got);
        let back = Skipped::parse(&s.serialize()).unwrap();
        assert_eq!(
            back.entries().collect::<Vec<_>>(),
            [(b".h\xff.rs".to_vec(), IGNORED)]
        );
        let _ = fs::remove_dir_all(&base);
    }
}
