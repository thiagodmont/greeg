//! Ignore inputs that live outside the indexed tree: ancestor ignore files,
//! the repository's `info/exclude`, and the global git excludes file with the
//! configuration that names it. They decide which files the walk yields, so
//! the index records a digest of them and rebuilds when it changes. Ignore
//! files inside the tree are tracked as files instead (`format::is_ignore_file`).

use std::fs;
use std::path::{Path, PathBuf};

/// Digest of every input's path and content (or absence).
pub fn digest(root: &Path) -> String {
    let mut h = blake3::Hasher::new();
    for p in inputs(root) {
        h.update(p.as_os_str().as_encoded_bytes());
        match fs::read(&p) {
            Ok(bytes) => {
                h.update(&[1]);
                h.update(&(bytes.len() as u64).to_le_bytes());
                h.update(&bytes);
            }
            Err(_) => {
                h.update(&[0]);
            }
        }
    }
    h.finalize().to_hex()[..32].to_string()
}

/// Candidate paths, present or not. A few extra paths only cost a failed
/// open; a missing one would leave a stale file set.
fn inputs(root: &Path) -> Vec<PathBuf> {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut v = Vec::new();
    for dir in root.ancestors().skip(1) {
        for name in [".gitignore", ".ignore", ".rgignore"] {
            v.push(dir.join(name));
        }
    }
    // which repository the root belongs to changes which rules apply at all
    if let Some(git) = git_dir(&root) {
        v.push(git.clone());
        v.push(git.join("info/exclude"));
        if let Ok(common) = fs::read_to_string(git.join("commondir")) {
            v.push(git.join(common.trim()).join("info/exclude"));
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|x| !x.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    let mut configs = Vec::new();
    if let Some(h) = &home {
        configs.push(h.join(".gitconfig"));
    }
    if let Some(x) = &xdg {
        configs.push(x.join("git/config"));
        v.push(x.join("git/ignore"));
    }
    for c in &configs {
        if let Ok(text) = fs::read_to_string(c) {
            v.extend(excludes_files(&text, home.as_deref()));
        }
    }
    v.extend(configs);
    v
}

/// The git directory of the repository containing `root`: `.git` itself, or
/// the target of a `.git` file (worktrees, submodules).
fn git_dir(root: &Path) -> Option<PathBuf> {
    for dir in root.ancestors() {
        let dot = dir.join(".git");
        let Ok(md) = fs::metadata(&dot) else {
            continue;
        };
        if md.is_dir() {
            return Some(dot);
        }
        let text = fs::read_to_string(&dot).ok()?;
        let target = text.trim().strip_prefix("gitdir:")?.trim();
        return Some(dir.join(target));
    }
    None
}

/// `excludesFile = PATH` values in a git config, `~/` expanded. Matched
/// loosely (any section): an extra path is harmless.
fn excludes_files(config: &str, home: Option<&Path>) -> Vec<PathBuf> {
    config
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            if !key.trim().eq_ignore_ascii_case("excludesfile") {
                return None;
            }
            let value = value.trim().trim_matches('"');
            Some(match (value.strip_prefix("~/"), home) {
                (Some(rest), Some(h)) => h.join(rest),
                _ => PathBuf::from(value),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_file_values_are_found_and_expanded() {
        let cfg = "[core]\n\texcludesFile = ~/.gitignore_global\n[user]\n\tname = x\n\
                   [core]\n  excludesfile=\"/etc/ignore\"\n";
        assert_eq!(
            excludes_files(cfg, Some(Path::new("/home/u"))),
            [
                PathBuf::from("/home/u/.gitignore_global"),
                PathBuf::from("/etc/ignore")
            ]
        );
    }

    #[test]
    fn digest_follows_exclude_content_and_worktree_links() {
        let base = std::env::temp_dir().join(format!("greeg-ignores-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let repo = base.join("repo");
        fs::create_dir_all(repo.join(".git/info")).unwrap();
        let before = digest(&repo);
        fs::write(repo.join(".git/info/exclude"), "a.txt\n").unwrap();
        let excluded = digest(&repo);
        assert_ne!(before, excluded);
        assert_eq!(excluded, digest(&repo));

        let wt = base.join("wt");
        fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        fs::write(repo.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        fs::create_dir_all(&wt).unwrap();
        fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", repo.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        let linked = digest(&wt);
        fs::write(repo.join(".git/info/exclude"), "b.txt\n").unwrap();
        assert_ne!(linked, digest(&wt));
        let _ = fs::remove_dir_all(&base);
    }
}
