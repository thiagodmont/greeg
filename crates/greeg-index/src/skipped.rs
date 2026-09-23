//! What the index leaves out inside the directories it walks: hidden files
//! and directories, and entries an ignore rule excludes. A request can still
//! select some of them (ripgrep lets a positive glob select a hidden or an
//! ignored file, and a type select a hidden one), so a query checks this
//! record before trusting the index to cover the request.
//!
//! skipped.<gen>.bin (build) and delta/NNNN.skipped (a refresh that changed
//! it); the manifest names the current one.
//!   u32 n, then n × (u8 bits, u32 len, path bytes), sorted by path

use crate::format::{self, is_ignore_file};
use anyhow::{Context, Result, bail};
use hashbrown::HashSet;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// A directory (otherwise a regular file).
pub const DIR: u8 = 1;
/// Excluded by an ignore rule (otherwise only for its hidden name, or a
/// tracked-only ignore file).
pub const IGNORED: u8 = 2;

/// One directory's skipped children: (name, bits).
pub type Children = Vec<(String, u8)>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Skipped {
    /// Walked directory → its skipped children.
    by_dir: BTreeMap<String, Children>,
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

impl Skipped {
    /// Replace one walked directory's skipped children.
    pub fn set(&mut self, dir: &str, mut kids: Children) {
        kids.sort();
        if kids.is_empty() {
            self.by_dir.remove(dir);
        } else {
            self.by_dir.insert(dir.to_string(), kids);
        }
    }

    pub fn update(&mut self, dirs: &[(String, Children)]) {
        for (d, kids) in dirs {
            self.set(d, kids.clone());
        }
    }

    /// Every skipped entry as (root-relative path, bits).
    pub fn entries(&self) -> impl Iterator<Item = (String, u8)> + '_ {
        self.by_dir
            .iter()
            .flat_map(|(d, kids)| kids.iter().map(move |(n, b)| (join(d, n), *b)))
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let n: usize = self.by_dir.values().map(Vec::len).sum();
        out.extend_from_slice(&(n as u32).to_le_bytes());
        for (rel, bits) in self.entries() {
            out.push(bits);
            out.extend_from_slice(&(rel.len() as u32).to_le_bytes());
            out.extend_from_slice(rel.as_bytes());
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
            let rel = std::str::from_utf8(
                body.get(at + 5..at + 5 + len)
                    .context("skipped: truncated")?,
            )?;
            at += 5 + len;
            let (d, name) = rel.rsplit_once('/').unwrap_or(("", rel));
            s.by_dir
                .entry(d.to_string())
                .or_default()
                .push((name.to_string(), bits));
        }
        if at != body.len() {
            bail!("skipped: trailing bytes");
        }
        Ok(s)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        format::write_atomic_with(path, format::COMP_SKIPPED, &self.serialize(), false)
    }

    /// The record the manifest of the index in `dir` names. `None` when it
    /// names none (built before the record existed) or it cannot be read:
    /// coverage is then unknown.
    pub fn read(dir: &Path, name: &str) -> Option<Skipped> {
        if name.is_empty() || name.starts_with('/') || name.split('/').any(|c| c == "..") {
            return None;
        }
        let bytes = fs::read(dir.join(name)).ok()?;
        Skipped::parse(format::check_header(&bytes, format::COMP_SKIPPED).ok()?).ok()
    }
}

/// The skipped children of each walked directory in `dirs`: every regular
/// file or directory in it that the walk did not keep. `kept` holds the
/// root-relative paths the walk yielded; `hidden` those it saw and left out
/// for their name. Anything else was excluded by an ignore rule before the
/// walk's filter saw it. Ignore files are yielded but never searched, so
/// they count as skipped hidden files. Lists on a few threads.
pub fn list(
    root: &Path,
    dirs: &[&str],
    kept: &HashSet<&str>,
    hidden: &HashSet<&str>,
) -> Vec<(String, Children)> {
    const THREADS: usize = 4;
    let chunk = dirs.len().div_ceil(THREADS).max(1);
    std::thread::scope(|sc| {
        let parts: Vec<_> = dirs
            .chunks(chunk)
            .map(|part| {
                sc.spawn(move || {
                    part.iter()
                        .filter_map(|d| Some((d.to_string(), children(root, d, kept, hidden)?)))
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

fn children(
    root: &Path,
    d: &str,
    kept: &HashSet<&str>,
    hidden: &HashSet<&str>,
) -> Option<Children> {
    let rd = fs::read_dir(if d.is_empty() {
        root.to_path_buf()
    } else {
        root.join(d)
    })
    .ok()?;
    let mut kids = Vec::new();
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if !ft.is_dir() && !ft.is_file() {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let rel = join(d, &name);
        let yielded = kept.contains(rel.as_str());
        if yielded && !(ft.is_file() && is_ignore_file(&name)) {
            continue;
        }
        let mut bits = if ft.is_dir() { DIR } else { 0 };
        if !yielded && !hidden.contains(rel.as_str()) {
            bits |= IGNORED;
        }
        kids.push((name, bits));
    }
    kids.sort();
    Some(kids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_replace_per_directory() {
        let mut s = Skipped::default();
        s.set(
            "",
            vec![(".env".into(), 0), ("target".into(), DIR | IGNORED)],
        );
        s.set("src", vec![("gen.rs".into(), IGNORED)]);
        let back = Skipped::parse(&s.serialize()).unwrap();
        assert_eq!(back, s);
        assert_eq!(
            back.entries().collect::<Vec<_>>(),
            [
                (".env".to_string(), 0),
                ("target".to_string(), DIR | IGNORED),
                ("src/gen.rs".to_string(), IGNORED)
            ]
        );
        let mut s2 = back;
        s2.update(&[("src".into(), vec![]), ("".into(), vec![(".x".into(), 0)])]);
        assert_eq!(s2.entries().collect::<Vec<_>>(), [(".x".to_string(), 0)]);
        assert!(Skipped::parse(&[1, 0, 0, 0]).is_err());
        assert!(Skipped::read(Path::new("/"), "../x").is_none());
    }
}
