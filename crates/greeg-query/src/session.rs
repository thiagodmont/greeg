//! Session memory: a small per-agent log of what was asked
//! and shown, used for dedup of already-shown context, loop detection, and
//! the focus set that biases ranking. Storage: `session/<id>.jsonl` under the
//! index directory. Storage and retention are bounded independently of search output.

use crate::session_store::Store;
use crate::shape::Report;
use crate::{Mode, Options, ScanResult};
use greeg_index::rel::key;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
#[cfg(any(test, target_os = "linux"))]
use std::fs;
#[cfg(test)]
use std::path::PathBuf;

pub(crate) const MAX_RECORDS: usize = 2000;
pub(crate) const MAX_AGE_SECS: u64 = 24 * 3600;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Record {
    /// Unix seconds.
    pub t: u64,
    /// Normalized query: sorted unique lowercase tokens.
    pub q: String,
    #[serde(default, skip_serializing)]
    pub pat: String,
    pub hits: usize,
    /// Files shown (relative paths, as `greeg_index::rel::key` spells them).
    pub files: Vec<String>,
    /// Legacy (file, line) pairs; superseded by `shown`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ctx: Vec<(String, u32)>,
    /// Line ranges whose context or block was printed: (file, first, last, mtime).
    #[serde(default)]
    pub shown: Vec<(String, u32, u32, u64)>,
}

/// Session dedup applies only to implicit context: never when the user asked
/// for `-A/-B/-C`, a block, or `--all` (C12).
pub fn dedup_applies(o: &Options) -> bool {
    !o.explicit_context() && o.mode != Mode::Block && !o.all
}

/// A previously printed range covers the requested one (same file and mtime).
pub fn covered(
    shown: &[(String, u32, u32, u64)],
    file: &str,
    first: u32,
    last: u32,
    mtime: u64,
) -> bool {
    shown
        .iter()
        .any(|(f, a, b, m)| f == file && *m == mtime && *a <= first && *b >= last)
}

pub struct Session {
    store: Store,
    #[cfg(test)]
    path: PathBuf,
    id: String,
    records: Vec<Record>,
}

/// Lowercase split tokens of a query, sorted and deduplicated.
pub fn normalize(q: &str) -> String {
    let mut toks: Vec<String> = greeg_index::symtab::split_tokens(q);
    if toks.is_empty() {
        toks = q
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
    }
    toks.sort();
    toks.dedup();
    toks.join(" ")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(target_os = "macos")]
fn parent_of(pid: u32) -> Option<(u32, String)> {
    use std::mem;
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: proc_pidinfo writes at most `size` bytes into `info`; a short return means failure.
    let r = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if r != size {
        return None;
    }
    let comm = unsafe { std::ffi::CStr::from_ptr(info.pbi_comm.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    Some((info.pbi_ppid, comm))
}

#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<(u32, String)> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = s.split_once(" (")?;
    let (comm, rest) = rest.rsplit_once(") ")?;
    let mut it = rest.split_whitespace();
    let _state = it.next()?;
    let ppid: u32 = it.next()?.parse().ok()?;
    Some((ppid, comm.to_string()))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn parent_of(_pid: u32) -> Option<(u32, String)> {
    None
}

fn is_shell(comm: &str) -> bool {
    let c = comm
        .rsplit('/')
        .next()
        .unwrap_or(comm)
        .trim_start_matches('-');
    matches!(
        c,
        "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "tcsh" | "csh" | "nu" | "xonsh" | "login"
    )
}

/// The first non-shell ancestor's pid: all calls from one agent process share it.
pub fn agent_pid() -> u32 {
    let me = std::process::id();
    let Some((mut pid, _)) = parent_of(me) else {
        return me;
    };
    for _ in 0..8 {
        let Some((ppid, comm)) = parent_of(pid) else {
            break;
        };
        if !is_shell(&comm) || ppid <= 1 {
            return pid;
        }
        pid = ppid;
    }
    pid
}

impl Session {
    /// Open (or create) the session for this query. `None` when no index
    /// directory can be determined or the private store cannot be accessed.
    pub fn open(o: &Options, id: Option<&str>) -> Option<Session> {
        let dir = greeg_index::repo_dir(&o.root, o.index_dir.as_deref())
            .ok()?
            .join("session");
        let id = id
            .map(str::to_owned)
            .or_else(|| {
                std::env::var("GREEG_SESSION")
                    .ok()
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| format!("p{}", agent_pid()));
        let key = if !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            id.clone()
        } else {
            format!("h-{}", blake3::hash(id.as_bytes()).to_hex())
        };
        let store = Store::open(&dir, &key).ok()?;
        let records = store.load(now_secs()).ok()?.records;
        Some(Session {
            #[cfg(test)]
            path: dir.join(format!("{key}.jsonl")),
            store,
            id,
            records,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Recently shown files, most recent first (the ranking focus set).
    pub fn focus(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for r in self.records.iter().rev() {
            for f in &r.files {
                if !out.contains(f) {
                    out.push(f.clone());
                    if out.len() >= 12 {
                        return out;
                    }
                }
            }
        }
        out
    }

    /// Drop implicit context whose range was already printed this session
    /// (same file, same mtime, covering range) and mark the hit.
    pub fn dedup(&self, r: &ScanResult, rep: &mut Report) {
        if self.records.is_empty() || !dedup_applies(&r.opts) {
            return;
        }
        let shown: Vec<(String, u32, u32, u64)> = self
            .records
            .iter()
            .flat_map(|r| r.shown.iter().cloned())
            .collect();
        if shown.is_empty() {
            return;
        }
        for sf in &mut rep.files {
            let f = &r.files[sf.file];
            for sh in &mut sf.hits {
                if let Some((first, lines)) = &sh.context {
                    let last = first + lines.len().saturating_sub(1) as u32;
                    if covered(&shown, &key(&f.rel), *first, last, f.mtime) {
                        sh.context = None;
                        sh.seen_before = true;
                    }
                }
            }
        }
    }

    /// Loop detection: the same token set ≥ 3 times in the last 5 queries.
    pub fn loop_hint(&self, o: &Options, r: &ScanResult, rep: &mut Report) {
        let q = normalize(&o.pattern);
        if q.is_empty() {
            return;
        }
        let recent = self.records.iter().rev().take(5);
        let same = recent.filter(|rec| rec.q == q).count();
        if same >= 2 {
            let shown: HashSet<&str> = self
                .records
                .iter()
                .flat_map(|r| r.files.iter().map(|s| s.as_str()))
                .collect();
            let new_files = r
                .files
                .iter()
                .filter(|f| !shown.contains(&*key(&f.rel)))
                .count();
            let ident = o
                .pattern
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .filter(|s| !s.is_empty())
                .max_by_key(|s| s.len())
                .unwrap_or(&o.pattern);
            let msg = if new_files == 0 {
                format!(
                    "this is query #{} for these tokens in this session with nothing new; try `greeg def {}`, `greeg map`, or different tokens",
                    same + 1,
                    ident
                )
            } else {
                format!(
                    "query #{} for these tokens this session ({} new files)",
                    same + 1,
                    new_files
                )
            };
            rep.footer.hints.insert(0, msg);
        }
    }

    /// Append this query's record (best effort; never fails the query).
    pub fn record(&self, o: &Options, r: &ScanResult, rep: &Report) {
        let mut files: Vec<String> = Vec::new();
        let mut shown: Vec<(String, u32, u32, u64)> = Vec::new();
        for sf in &rep.files {
            let f = &r.files[sf.file];
            let rel = key(&f.rel).into_owned();
            if !files.contains(&rel) {
                files.push(rel.clone());
            }
            for sh in &sf.hits {
                for (first, lines) in sh.context.iter().chain(sh.block.iter()) {
                    if !lines.is_empty() {
                        shown.push((rel.clone(), *first, first + lines.len() as u32 - 1, f.mtime));
                    }
                }
            }
        }
        files.truncate(64);
        shown.truncate(128);
        let rec = Record {
            t: now_secs(),
            q: normalize(&o.pattern),
            pat: String::new(),
            hits: r.stats.total_hits,
            files,
            ctx: Vec::new(),
            shown,
        };
        let _ = self.store.append(&rec, now_secs());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = loop {
                let candidate = std::env::temp_dir().join(format!(
                    "greeg-session-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                match fs::create_dir(&candidate) {
                    Ok(()) => break candidate,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("fixture directory: {e}"),
                }
            };
            Self(path)
        }

        fn options(&self) -> Options {
            Options {
                root: self.0.clone(),
                index_dir: Some(self.0.join("index")),
                ..Options::default()
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn expired_records_do_not_affect_focus() {
        let f = Fixture::new();
        let o = f.options();
        let s = Session::open(&o, Some("expiry")).unwrap();
        fs::create_dir_all(s.path.parent().unwrap()).unwrap();
        let expired = Record {
            t: now_secs() - MAX_AGE_SECS - 1,
            files: vec!["old.rs".into()],
            ..Record::default()
        };
        let live = Record {
            t: now_secs(),
            files: vec!["new.rs".into()],
            ..Record::default()
        };
        fs::write(
            &s.path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&expired).unwrap(),
                serde_json::to_string(&live).unwrap()
            ),
        )
        .unwrap();
        assert_eq!(
            Session::open(&o, Some("expiry")).unwrap().focus(),
            ["new.rs"]
        );
    }

    #[test]
    fn session_ids_do_not_collide_after_sanitization() {
        let f = Fixture::new();
        let o = f.options();
        let ids = ["a/b", "ab", "!!!", "???", "", "long"];
        let paths: HashSet<_> = ids
            .iter()
            .map(|id| Session::open(&o, Some(id)).unwrap().path)
            .collect();
        assert_eq!(paths.len(), ids.len());
        let long = "a".repeat(64);
        assert_ne!(
            Session::open(&o, Some(&format!("{long}x"))).unwrap().path,
            Session::open(&o, Some(&format!("{long}y"))).unwrap().path
        );
    }

    #[test]
    fn symlink_records_are_not_loaded() {
        let f = Fixture::new();
        let o = f.options();
        let s = Session::open(&o, Some("symlink")).unwrap();
        fs::create_dir_all(s.path.parent().unwrap()).unwrap();
        let target = f.0.join("target");
        let record = Record {
            t: now_secs(),
            files: vec!["outside.rs".into()],
            ..Record::default()
        };
        fs::write(&target, serde_json::to_vec(&record).unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &s.path).unwrap();
        assert!(Session::open(&o, Some("symlink")).is_none_or(|s| s.focus().is_empty()));
    }

    #[test]
    fn normalize_tokens() {
        assert_eq!(normalize("getUserName"), "get name user");
        assert_eq!(normalize("fn poll"), "poll");
        assert_eq!(normalize("user_name"), "name user");
    }

    #[test]
    fn agent_pid_is_some_process() {
        assert!(agent_pid() > 0);
    }

    #[test]
    fn dedup_rules() {
        let shown = vec![("a.rs".to_string(), 10, 20, 7u64)];
        // covered only when the earlier range contains the requested one, same mtime
        assert!(covered(&shown, "a.rs", 12, 18, 7));
        assert!(covered(&shown, "a.rs", 10, 20, 7));
        assert!(
            !covered(&shown, "a.rs", 8, 18, 7),
            "more context than before is shown again"
        );
        assert!(!covered(&shown, "a.rs", 12, 25, 7));
        assert!(
            !covered(&shown, "a.rs", 12, 18, 8),
            "edited file (mtime) is shown again"
        );
        assert!(!covered(&shown, "b.rs", 12, 18, 7));
        // explicit context, block mode and --all are never deduplicated
        let mut o = Options::default();
        assert!(dedup_applies(&o));
        o.context = Some(2);
        assert!(!dedup_applies(&o));
        o = Options {
            after: 3,
            ..Default::default()
        };
        assert!(!dedup_applies(&o));
        o = Options {
            before: 1,
            ..Default::default()
        };
        assert!(!dedup_applies(&o));
        o = Options {
            mode: Mode::Block,
            ..Default::default()
        };
        assert!(!dedup_applies(&o));
        o = Options {
            all: true,
            ..Default::default()
        };
        assert!(!dedup_applies(&o));
    }

    #[test]
    fn old_records_still_parse() {
        let r: Record = serde_json::from_str(
            r#"{"t":1,"q":"a","pat":"a","hits":1,"files":[],"ctx":[["a.rs",3]]}"#,
        )
        .unwrap();
        assert!(r.shown.is_empty());
        assert_eq!(r.ctx.len(), 1);
    }
}
