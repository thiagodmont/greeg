//! `greeg purge`: remove what greeg keeps for repositories, and only what it
//! wrote. Each index, pre-0.8 index and session directory goes under its own
//! lock; one that is in use is reported and kept.

use crate::legacy;
use std::fs;
use std::path::{Path, PathBuf};

/// Where to purge.
pub enum Scope {
    /// Every repository directory greeg owns under its cache directory, and
    /// the statistics there.
    Cache(PathBuf),
    /// The owned entries of a directory someone chose (`--index-dir`),
    /// never the directory itself.
    Chosen(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Found and free; kept by a preview.
    Found,
    /// Its lock is held: a build, refresh or session write is running.
    Busy,
    Removed,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Index,
    /// The top-level files of a release before 0.8.
    TopLevel,
    Sessions,
    Stats,
}

pub struct Item {
    pub path: PathBuf,
    pub kind: Kind,
    pub bytes: u64,
    pub state: State,
}

/// Find what `scope` holds and, with `remove`, remove what is free.
pub fn purge(scope: &Scope, remove: bool) -> Vec<Item> {
    let mut items = Vec::new();
    match scope {
        Scope::Cache(base) => {
            let mut names = legacy::entries(base);
            names.sort();
            for name in names {
                let path = base.join(&name);
                if name == "stats" && real_dir(&path) {
                    items.push(purge_one(path, Kind::Stats, Some(".lock"), remove));
                } else if repo_name(&name) && owned_repo(&path) {
                    let found = purge_repo(&path, remove, &mut items);
                    // emptied, and holding nothing greeg did not write
                    if remove && found.iter().all(|s| *s == State::Removed) {
                        let _ = fs::remove_dir(&path);
                    }
                }
            }
        }
        Scope::Chosen(dir) => {
            if owned_repo(dir) {
                purge_repo(dir, remove, &mut items);
            }
        }
    }
    items
}

/// The owned entries of one repository directory; returns their states.
fn purge_repo(repo: &Path, remove: bool, items: &mut Vec<Item>) -> Vec<State> {
    let start = items.len();
    let mut names = legacy::entries(repo);
    names.sort();
    for name in &names {
        let path = repo.join(name);
        if legacy::layout_number(name).is_some() && legacy::owned_layout(&path) {
            items.push(purge_one(path, Kind::Index, Some("LOCK"), remove));
        }
    }
    let top: Vec<&String> = names.iter().filter(|n| legacy::is_top_level(n)).collect();
    if !top.is_empty() && legacy::top_level_verified(repo).is_some() {
        let bytes = top.iter().map(|n| size(&repo.join(n))).sum();
        let state = match legacy::try_lock(repo) {
            None => State::Busy,
            Some(_) if !remove => State::Found,
            Some(_lock) if legacy::remove_top_level(repo, &top) == top.len() => State::Removed,
            Some(_) => State::Failed,
        };
        items.push(Item {
            path: repo.to_path_buf(),
            kind: Kind::TopLevel,
            bytes,
            state,
        });
    }
    let sessions = repo.join("session");
    if real_dir(&sessions) {
        items.push(purge_one(sessions, Kind::Sessions, Some(".lock"), remove));
    }
    items[start..].iter().map(|i| i.state).collect()
}

/// One directory, removed while its lock file `lock` is held.
fn purge_one(path: PathBuf, kind: Kind, lock: Option<&str>, remove: bool) -> Item {
    let bytes = size(&path);
    let held = match lock {
        Some(name) => legacy::try_lock_file(&path.join(name)),
        None => Some(None),
    };
    let state = match held {
        None => State::Busy,
        Some(_) if !remove => State::Found,
        Some(_lock) => {
            let gone = if kind == Kind::Index {
                legacy::remove_layout(&path)
            } else {
                fs::remove_dir_all(&path).is_ok()
            };
            if gone { State::Removed } else { State::Failed }
        }
    };
    Item {
        path,
        kind,
        bytes,
        state,
    }
}

/// `<name>-<16 hex>`, as `repo_dir_name` makes them.
fn repo_name(name: &str) -> bool {
    let Some((prefix, hex)) = name.rsplit_once('-') else {
        return false;
    };
    hex.len() == 16
        && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && prefix.len() <= 40
        && prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A real directory holding a greeg index: a layout with `OWNER`, or a
/// pre-0.8 index its manifest vouches for.
fn owned_repo(dir: &Path) -> bool {
    real_dir(dir)
        && (legacy::entries(dir)
            .iter()
            .any(|n| legacy::layout_number(n).is_some() && legacy::owned_layout(&dir.join(n)))
            || legacy::top_level_verified(dir).is_some())
}

fn real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// Bytes of the files below `p`, not following symlinks.
fn size(p: &Path) -> u64 {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => legacy::entries(p).iter().map(|n| size(&p.join(n))).sum(),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn base(name: &str) -> PathBuf {
        (0..)
            .map(|n| {
                std::env::temp_dir().join(format!("greeg-purge-{name}-{}-{n}", std::process::id()))
            })
            .find(|d| match fs::create_dir(d) {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
                Err(e) => panic!("create {}: {e}", d.display()),
            })
            .unwrap()
    }

    fn write(p: &Path, body: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn layout(repo: &Path, n: u32) {
        write(
            &repo.join(format!("v{n}/OWNER")),
            &format!("greeg 0.8.0 {n}\n"),
        );
        write(&repo.join(format!("v{n}/LOCK")), "");
        write(
            &repo.join(format!("v{n}/g-0000000000000001/files.bin")),
            "data",
        );
    }

    fn top_level(repo: &Path) {
        write(
            &repo.join("manifest"),
            r#"{"format":5,"generation":2,"verified_unix_ms":1}"#,
        );
        for f in ["LOCK", "files.2.bin", "grams.2.bin"] {
            write(&repo.join(f), "data");
        }
    }

    #[test]
    fn a_preview_removes_nothing_and_yes_removes_only_owned_paths() {
        let b = base("cache");
        let owned = b.join("django-0123456789abcdef");
        layout(&owned, 11);
        layout(&owned, 6);
        write(&owned.join("session/p1.jsonl"), "{}\n");
        write(&owned.join("notes.txt"), "mine\n");
        let old = b.join("tokio-fedcba9876543210");
        top_level(&old);
        write(&old.join("session/.lock"), "");
        let bare = b.join("gone-00112233445566aa");
        layout(&bare, 11);
        // a name like greeg's with nothing greeg wrote, and a foreign name
        write(&b.join("other-0123456789abcdef/manifest"), "{}");
        layout(&b.join("not-a-repo"), 11);
        write(&b.join("stats/events.jsonl"), "{}\n");
        let scope = Scope::Cache(b.clone());

        let preview = purge(&scope, false);
        let found: Vec<(Kind, &Path)> =
            preview.iter().map(|i| (i.kind, i.path.as_path())).collect();
        assert_eq!(
            found,
            [
                (Kind::Index, owned.join("v11").as_path()),
                (Kind::Index, owned.join("v6").as_path()),
                (Kind::Sessions, owned.join("session").as_path()),
                (Kind::Index, bare.join("v11").as_path()),
                (Kind::Stats, b.join("stats").as_path()),
                (Kind::TopLevel, old.as_path()),
                (Kind::Sessions, old.join("session").as_path()),
            ]
        );
        assert!(preview.iter().all(|i| i.state == State::Found));
        assert!(owned.join("v11/OWNER").exists() && b.join("stats").exists());

        let done = purge(&scope, true);
        assert!(done.iter().all(|i| i.state == State::Removed));
        let mut left = legacy::entries(&b);
        left.sort();
        assert_eq!(
            left,
            [
                "django-0123456789abcdef",
                "not-a-repo",
                "other-0123456789abcdef"
            ]
        );
        assert_eq!(legacy::entries(&owned), ["notes.txt"]);
        assert!(purge(&scope, false).is_empty());
        let _ = fs::remove_dir_all(&b);
    }

    #[test]
    fn an_index_in_use_is_kept_and_a_chosen_directory_stays() {
        let b = base("chosen");
        layout(&b, 11);
        layout(&b, 12);
        write(&b.join("keep.txt"), "mine\n");
        let held = File::options()
            .write(true)
            .open(b.join("v12/LOCK"))
            .unwrap();
        held.lock().unwrap();
        let done = purge(&Scope::Chosen(b.clone()), true);
        let states: Vec<(PathBuf, State)> =
            done.iter().map(|i| (i.path.clone(), i.state)).collect();
        assert_eq!(
            states,
            [
                (b.join("v11"), State::Removed),
                (b.join("v12"), State::Busy)
            ]
        );
        assert!(b.join("v12/OWNER").exists() && b.join("keep.txt").exists());
        drop(held);
        assert_eq!(
            purge(&Scope::Chosen(b.clone()), true)[0].state,
            State::Removed
        );
        assert!(b.exists());
        let _ = fs::remove_dir_all(&b);
    }

    #[test]
    fn only_names_repo_dir_name_makes_are_repositories() {
        for n in [
            "django-0123456789abcdef",
            "a_b-c-0123456789abcdef",
            "-0123456789abcdef",
        ] {
            assert!(repo_name(n), "{n}");
        }
        for n in [
            "stats",
            "django-0123456789ABCDEF",
            "django-0123456789abcde",
            "django_0123456789abcdef",
            "dj.ango-0123456789abcdef",
            &format!("{}-0123456789abcdef", "x".repeat(41)),
        ] {
            assert!(!repo_name(n), "{n}");
        }
    }
}
