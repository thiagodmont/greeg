//! Data other layouts left in a repository directory: releases before 0.8
//! kept one index at its top level, and each layout since keeps `v<N>/`
//! (ARCHITECTURE.md). It goes once nothing has verified it for
//! [`UNVERIFIED_MS`], and is never read.

use crate::private::PrivateDir;
use std::fs::{self, File};
use std::path::Path;
use std::sync::Once;

/// How long another layout's index may go unverified before it is removed:
/// a binary that still uses it verifies it again on every refresh.
pub const UNVERIFIED_MS: u64 = 14 * 24 * 3600 * 1000;

/// The last layout that kept its index at the top of the repository directory.
const TOP_LEVEL_FORMAT: u64 = 5;

const TOP_LEVEL_COMPONENTS: [&str; 7] = [
    "files", "grams", "words", "symbols", "spans", "graph", "skipped",
];

/// Tighten greeg's default cache directory, and `repo` when it lies in it,
/// to 0700, once per process. An older release left them readable, and
/// without traversal the files it left are out of other users' reach. What
/// this user does not own or what carries an ACL is left alone, and so is a
/// directory someone chose.
pub fn secure(repo: &Path) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let Ok(base) = crate::cache_base() else {
            return;
        };
        if repo.parent() != Some(base.as_path()) {
            return;
        }
        let (Some(parent), Some(name), Some(repo_name)) =
            (base.parent(), base.file_name(), repo.file_name())
        else {
            return;
        };
        if PrivateDir::open(parent, name).is_ok() {
            let _ = PrivateDir::open(&base, repo_name);
        }
    });
}

/// Remove the indexes of other layouts in `repo` that nothing has verified
/// for [`UNVERIFIED_MS`]: the top-level files of releases before 0.8 when a
/// manifest of theirs says so, and each older `v<N>/` that carries `OWNER`.
/// One whose writer lock is held is kept. Newer layouts, sessions and
/// anything greeg did not write stay. Returns how many entries went.
pub fn clean(repo: &Path, now_ms: u64) -> usize {
    let mut removed = 0;
    let stale = |verified: u64| now_ms.saturating_sub(verified) >= UNVERIFIED_MS;
    let top: Vec<String> = entries(repo)
        .into_iter()
        .filter(|n| is_top_level(n))
        .collect();
    if let Some(verified) = top_level_verified(repo)
        && stale(verified)
        && let Some(_lock) = try_lock(repo)
    {
        // the lock file goes last, while it is still held
        let (lock, rest): (Vec<_>, Vec<_>) = top.iter().partition(|n| *n == "LOCK");
        for name in rest.into_iter().chain(lock) {
            removed += usize::from(remove(&repo.join(name)));
        }
    }
    for name in entries(repo) {
        let Some(n) = layout_number(&name) else {
            continue;
        };
        let dir = repo.join(&name);
        if n >= crate::FORMAT_VERSION || !owned_layout(&dir) {
            continue;
        }
        if stale(layout_verified(&dir)) && try_lock(&dir).is_some() {
            removed += usize::from(fs::remove_dir_all(&dir).is_ok());
        }
    }
    removed
}

fn entries(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

/// A name the top-level layouts wrote.
fn is_top_level(name: &str) -> bool {
    let generation_bin = |prefix: &str| {
        name.strip_prefix(prefix)
            .and_then(|r| r.strip_prefix('.'))
            .and_then(|r| r.strip_suffix(".bin"))
            .is_some_and(|g| !g.is_empty() && g.bytes().all(|b| b.is_ascii_digit()))
    };
    let pid_dir = name
        .strip_prefix("build-tmp.")
        .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    matches!(
        name,
        "manifest" | "LOCK" | "BUILDING" | "REFRESHING" | "delta"
    ) || name.ends_with(".tmp")
        || pid_dir
        || TOP_LEVEL_COMPONENTS.iter().any(|p| generation_bin(p))
}

/// When a top-level index was last verified, from its manifest; `None` when
/// there is no manifest of a top-level layout, so nothing shows the files
/// are greeg's.
fn top_level_verified(repo: &Path) -> Option<u64> {
    let m = manifest(repo)?;
    let format = m.get("format")?.as_u64()?;
    (format <= TOP_LEVEL_FORMAT && m.get("generation").is_some())
        .then(|| m.get("verified_unix_ms")?.as_u64())
        .flatten()
}

fn manifest(dir: &Path) -> Option<serde_json::Value> {
    let bytes = fs::read(dir.join("manifest")).ok()?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .filter(serde_json::Value::is_object)
}

/// `N` of a `v<N>` directory name.
fn layout_number(name: &str) -> Option<u32> {
    let n = name.strip_prefix('v')?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok()
}

/// A real directory whose `OWNER` a greeg layout wrote.
fn owned_layout(dir: &Path) -> bool {
    fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
        && fs::read(dir.join("OWNER")).is_ok_and(|o| o.starts_with(b"greeg "))
}

/// When a layout's index was last verified: its manifest, or the directory's
/// own time when it has none.
fn layout_verified(dir: &Path) -> u64 {
    manifest(dir)
        .and_then(|m| m.get("verified_unix_ms")?.as_u64())
        .or_else(|| {
            let t = fs::symlink_metadata(dir).ok()?.modified().ok()?;
            Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64)
        })
        .unwrap_or(u64::MAX)
}

/// The writer lock of the layout in `dir`, when it is free (or absent).
fn try_lock(dir: &Path) -> Option<Option<File>> {
    match File::options()
        .read(true)
        .write(true)
        .open(dir.join("LOCK"))
    {
        Ok(f) => f.try_lock().ok().map(|()| Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    }
}

fn remove(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path).is_ok(),
        Ok(_) => fs::remove_file(path).is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const DAY_MS: u64 = 24 * 3600 * 1000;
    const NOW: u64 = 100 * DAY_MS;

    fn repo(name: &str) -> PathBuf {
        let d = (0..)
            .map(|n| {
                std::env::temp_dir().join(format!("greeg-legacy-{name}-{}-{n}", std::process::id()))
            })
            .find(|d| match fs::create_dir(d) {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
                Err(e) => panic!("create {}: {e}", d.display()),
            })
            .unwrap();
        fs::create_dir(d.join("session")).unwrap();
        fs::write(d.join("session/s.jsonl"), "{}\n").unwrap();
        d
    }

    fn top_level(repo: &Path, verified: u64) {
        let m = format!(r#"{{"format":5,"generation":3,"verified_unix_ms":{verified}}}"#);
        fs::write(repo.join("manifest"), m).unwrap();
        for f in [
            "LOCK",
            "BUILDING",
            "files.3.bin",
            "grams.3.bin",
            "skipped.3.bin",
            "x.tmp",
        ] {
            fs::write(repo.join(f), "").unwrap();
        }
        fs::create_dir_all(repo.join("delta")).unwrap();
        fs::write(repo.join("delta/0001.bin"), "").unwrap();
        fs::create_dir_all(repo.join("build-tmp.42/spill")).unwrap();
    }

    fn layout(repo: &Path, n: u32, verified: Option<u64>) -> PathBuf {
        let d = repo.join(format!("v{n}"));
        fs::create_dir_all(d.join("g-0000000000000001")).unwrap();
        fs::write(d.join("OWNER"), format!("greeg 0.8.0 {n}\n")).unwrap();
        fs::write(d.join("LOCK"), "").unwrap();
        if let Some(v) = verified {
            fs::write(
                d.join("manifest"),
                format!(r#"{{"format":{n},"verified_unix_ms":{v}}}"#),
            )
            .unwrap();
        }
        d
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v = entries(dir);
        v.sort();
        v
    }

    #[test]
    fn a_top_level_index_unverified_for_two_weeks_goes_and_nothing_else() {
        let r = repo("top");
        top_level(&r, NOW - 15 * DAY_MS);
        fs::write(r.join("notes.txt"), "mine\n").unwrap();
        let current = layout(&r, crate::FORMAT_VERSION, Some(NOW - 30 * DAY_MS));
        assert_eq!(clean(&r, NOW), 9);
        assert_eq!(
            names(&r),
            [
                "notes.txt",
                "session",
                &format!("v{}", crate::FORMAT_VERSION)
            ]
        );
        assert!(current.join("OWNER").exists());
        assert!(r.join("session/s.jsonl").exists());
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn a_recently_verified_locked_or_unproven_top_level_index_stays() {
        let r = repo("top-kept");
        top_level(&r, NOW - 13 * DAY_MS);
        assert_eq!(clean(&r, NOW), 0, "verified within two weeks");
        top_level(&r, NOW - 15 * DAY_MS);
        let held = File::options().write(true).open(r.join("LOCK")).unwrap();
        held.lock().unwrap();
        assert_eq!(clean(&r, NOW), 0, "a writer holds it");
        drop(held);
        fs::write(r.join("manifest"), r#"{"format":6,"verified_unix_ms":1}"#).unwrap();
        assert_eq!(clean(&r, NOW), 0, "not a top-level layout's manifest");
        fs::remove_file(r.join("manifest")).unwrap();
        assert_eq!(clean(&r, NOW), 0, "nothing shows the files are greeg's");
        assert!(r.join("files.3.bin").exists());
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn older_layouts_go_once_unverified_and_newer_or_foreign_ones_stay() {
        let r = repo("layouts");
        let now = crate::now_ms();
        let old = layout(&r, 6, Some(now - 15 * DAY_MS));
        let recent = layout(&r, 7, Some(now - DAY_MS));
        let newer = layout(&r, crate::FORMAT_VERSION + 1, Some(now - 30 * DAY_MS));
        let current = layout(&r, crate::FORMAT_VERSION, Some(now - 30 * DAY_MS));
        // no manifest: the directory's own time, which is now
        let bare = layout(&r, 8, None);
        let foreign = r.join("v9");
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("manifest"), r#"{"verified_unix_ms":1}"#).unwrap();
        let busy = layout(&r, 10, Some(now - 15 * DAY_MS));
        let held = File::options().write(true).open(busy.join("LOCK")).unwrap();
        held.lock().unwrap();
        assert_eq!(clean(&r, now), 1);
        assert!(!old.exists());
        for kept in [&recent, &newer, &current, &bare, &foreign, &busy] {
            assert!(kept.exists(), "{}", kept.display());
        }
        drop(held);
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn only_names_the_layouts_wrote_count_as_top_level() {
        for n in [
            "manifest",
            "LOCK",
            "BUILDING",
            "REFRESHING",
            "delta",
            "a.tmp",
            "build-tmp.7",
            "files.12.bin",
            "graph.0.bin",
            "skipped.3.bin",
        ] {
            assert!(is_top_level(n), "{n}");
        }
        for n in [
            "session",
            "stats",
            "v11",
            "OWNER",
            "files.bin",
            "files.x.bin",
            "files.3.bin.old",
            "build-tmp.",
            "build-tmp.a",
            "notes.txt",
            "manifest.json",
        ] {
            assert!(!is_top_level(n), "{n}");
        }
        assert_eq!(layout_number("v10"), Some(10));
        for n in ["v", "vx", "v1a", "version"] {
            assert_eq!(layout_number(n), None, "{n}");
        }
    }
}
