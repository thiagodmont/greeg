//! Ignore inputs that live outside the indexed tree: ancestor ignore files,
//! the repository's `info/exclude`, and the global git excludes file with the
//! configuration that names it. They decide which files the walk yields, so
//! the index records a digest of them and rebuilds when it changes. Ignore
//! files inside the tree are tracked as files instead (`format::is_ignore_file`).

use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Digest of every input's path and identity: device, inode, size, mtime and
/// ctime. Any write or mtime reset moves ctime, so contents need not be read.
pub fn digest(root: &Path) -> String {
    let mut h = blake3::Hasher::new();
    for p in inputs(root) {
        h.update(p.as_os_str().as_encoded_bytes());
        match fs::metadata(&p) {
            Ok(md) => {
                h.update(&[1]);
                for v in [
                    md.dev(),
                    md.ino(),
                    md.size(),
                    md.mtime() as u64,
                    md.mtime_nsec() as u64,
                    md.ctime() as u64,
                    md.ctime_nsec() as u64,
                ] {
                    h.update(&v.to_le_bytes());
                }
            }
            Err(_) => {
                h.update(&[0]);
            }
        }
    }
    h.finalize().to_hex()[..32].to_string()
}

/// Candidate paths, present or not. A few extra paths only cost a failed
/// stat; a missing one would leave a stale file set.
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
        if let Some(common) = read_small(&git.join("commondir")) {
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
        if let Some(text) = read_small(c) {
            v.extend(excludes_files(&text, home.as_deref()));
        }
    }
    v.extend(configs);
    v
}

/// A small regular file's text. Symlinks are followed (dotfiles are often
/// linked), but a FIFO or device is never read and cannot block the open.
fn read_small(path: &Path) -> Option<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    f.take(1 << 20).read_to_string(&mut text).ok()?;
    Some(text)
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
        let text = read_small(&dot)?;
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
            let value = config_value(value);
            Some(match (value.strip_prefix("~/"), home) {
                (Some(rest), Some(h)) => h.join(rest),
                _ => PathBuf::from(value),
            })
        })
        .collect()
}

/// A git config value: `#` and `;` start a comment outside double quotes,
/// quotes are removed and `\"` / `\\` unescaped.
fn config_value(raw: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    let mut chars = raw.trim().chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' => out.extend(chars.next()),
            '#' | ';' if !quoted => break,
            _ => out.push(c),
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory this test creates, so cleanup never touches another's.
    fn own_dir(tag: &str) -> PathBuf {
        for n in 0.. {
            let d = std::env::temp_dir().join(format!("greeg-{tag}-{}-{n}", std::process::id()));
            match fs::create_dir(&d) {
                Ok(()) => return d,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create {}: {e}", d.display()),
            }
        }
        unreachable!()
    }

    #[test]
    fn excludes_file_values_follow_git_config_syntax() {
        let cfg = "[core]\n\texcludesFile = ~/.gitignore_global # personal\n[user]\n\tname = x\n\
                   [core]\n  excludesfile=\"/etc/my ignore;v2\" ; quoted\n\
                   excludesfile = /opt/a\\\"b\n";
        assert_eq!(
            excludes_files(cfg, Some(Path::new("/home/u"))),
            [
                PathBuf::from("/home/u/.gitignore_global"),
                PathBuf::from("/etc/my ignore;v2"),
                PathBuf::from("/opt/a\"b"),
            ]
        );
    }

    #[test]
    fn digest_follows_exclude_edits_and_worktree_links() {
        let base = own_dir("ignores");
        let repo = base.join("repo");
        fs::create_dir_all(repo.join(".git/info")).unwrap();
        let before = digest(&repo);
        let exclude = repo.join(".git/info/exclude");
        fs::write(&exclude, "a.txt\n").unwrap();
        let excluded = digest(&repo);
        assert_ne!(before, excluded);
        assert_eq!(excluded, digest(&repo));
        // same size, mtime restored: ctime still moves
        let mtime = fs::metadata(&exclude).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&exclude, "b.txt\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&exclude)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_ne!(excluded, digest(&repo));

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
        fs::write(&exclude, "c.txt\n").unwrap();
        assert_ne!(linked, digest(&wt));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn fifo_inputs_do_not_block() {
        let base = own_dir("ignores-fifo");
        let repo = base.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        for p in [repo.join(".git/commondir"), base.join(".gitignore")] {
            let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: a valid NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let r = repo.clone();
        std::thread::spawn(move || tx.send(digest(&r)).unwrap());
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "digest blocked on a FIFO"
        );
        let _ = fs::remove_dir_all(&base);
    }
}
