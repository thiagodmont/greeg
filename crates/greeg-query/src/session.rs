//! Session memory (DESIGN.md §9): a small per-agent log of what was asked
//! and shown, used for dedup of already-shown context, loop detection, and
//! the focus set that biases ranking. Storage: `session/<id>.jsonl` under the
//! index directory. Reading the log costs well under a millisecond.

use crate::shape::Report;
use crate::{Options, ScanResult};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

const MAX_RECORDS: usize = 2000;
const MAX_AGE_SECS: u64 = 24 * 3600;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Record {
    /// Unix seconds.
    pub t: u64,
    /// Normalized query: sorted unique lowercase tokens.
    pub q: String,
    pub pat: String,
    pub hits: usize,
    /// Files shown (relative paths).
    pub files: Vec<String>,
    /// (file, line) pairs whose context or block was shown.
    pub ctx: Vec<(String, u32)>,
}

pub struct Session {
    path: PathBuf,
    id: String,
    records: Vec<Record>,
}

/// Lowercase split tokens of a query, sorted and deduplicated.
pub fn normalize(q: &str) -> String {
    let mut toks: Vec<String> = greeg_index::symtab::split_tokens(q);
    if toks.is_empty() {
        toks = q.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
    }
    toks.sort();
    toks.dedup();
    toks.join(" ")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(target_os = "macos")]
fn parent_of(pid: u32) -> Option<(u32, String)> {
    use std::mem;
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: proc_pidinfo writes at most `size` bytes into `info`; a short return means failure.
    let r = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut libc::c_void, size) };
    if r != size {
        return None;
    }
    let comm = unsafe { std::ffi::CStr::from_ptr(info.pbi_comm.as_ptr()) }.to_string_lossy().into_owned();
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
    let c = comm.rsplit('/').next().unwrap_or(comm).trim_start_matches('-');
    matches!(c, "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "tcsh" | "csh" | "nu" | "xonsh" | "login")
}

/// The first non-shell ancestor's pid: all calls from one agent process share it.
pub fn agent_pid() -> u32 {
    let me = std::process::id();
    let Some((mut pid, _)) = parent_of(me) else { return me };
    for _ in 0..8 {
        let Some((ppid, comm)) = parent_of(pid) else { break };
        if !is_shell(&comm) || ppid <= 1 {
            return pid;
        }
        pid = ppid;
    }
    pid
}

impl Session {
    /// Open (or create) the session for this query. `None` when no index
    /// directory can be determined (session memory needs somewhere to live).
    pub fn open(o: &Options, id: Option<&str>) -> Option<Session> {
        let dir = match &o.index_dir {
            Some(d) => d.clone(),
            None => greeg_index::index_dir_for(&o.root).ok()?,
        }
        .join("session");
        let id = match id.map(|s| s.to_string()).or_else(|| std::env::var("GREEG_SESSION").ok()).filter(|s| !s.is_empty()) {
            Some(s) => s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(64).collect(),
            None => format!("p{}", agent_pid()),
        };
        let path = dir.join(format!("{id}.jsonl"));
        let mut records: Vec<Record> = Vec::new();
        if let Ok(s) = fs::read_to_string(&path) {
            for line in s.lines() {
                if let Ok(r) = serde_json::from_str::<Record>(line) {
                    records.push(r);
                }
            }
        }
        Some(Session { path, id, records })
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

    /// Drop context that was already shown this session and mark the hit.
    pub fn dedup(&self, r: &ScanResult, rep: &mut Report) {
        if self.records.is_empty() {
            return;
        }
        let seen: HashSet<(&str, u32)> = self.records.iter().flat_map(|r| r.ctx.iter().map(|(f, l)| (f.as_str(), *l))).collect();
        if seen.is_empty() {
            return;
        }
        for sf in &mut rep.files {
            let f = &r.files[sf.file];
            for sh in &mut sf.hits {
                let line = f.hits[sh.hit].line;
                if (sh.context.is_some() || sh.block.is_some()) && seen.contains(&(f.rel.as_str(), line)) {
                    sh.context = None;
                    sh.block = None;
                    sh.seen_before = true;
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
            let shown: HashSet<&str> = self.records.iter().flat_map(|r| r.files.iter().map(|s| s.as_str())).collect();
            let new_files = r.files.iter().filter(|f| !shown.contains(f.rel.as_str())).count();
            let ident = o.pattern.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|s| !s.is_empty()).max_by_key(|s| s.len()).unwrap_or(&o.pattern);
            let msg = if new_files == 0 {
                format!("this is query #{} for these tokens in this session with nothing new; try `greeg def {}`, `greeg map`, or different tokens", same + 1, ident)
            } else {
                format!("query #{} for these tokens this session ({} new files)", same + 1, new_files)
            };
            rep.footer.hints.insert(0, msg);
        }
    }

    /// Append this query's record (best effort; never fails the query).
    pub fn record(&self, o: &Options, r: &ScanResult, rep: &Report) {
        let mut files: Vec<String> = Vec::new();
        let mut ctx: Vec<(String, u32)> = Vec::new();
        for sf in &rep.files {
            let f = &r.files[sf.file];
            if !files.contains(&f.rel) {
                files.push(f.rel.clone());
            }
            for sh in &sf.hits {
                if sh.context.is_some() || sh.block.is_some() {
                    ctx.push((f.rel.clone(), f.hits[sh.hit].line));
                }
            }
        }
        files.truncate(64);
        ctx.truncate(128);
        let rec = Record { t: now_secs(), q: normalize(&o.pattern), pat: o.pattern.clone(), hits: r.stats.total_hits, files, ctx };
        let Ok(line) = serde_json::to_string(&rec) else { return };
        let _ = fs::create_dir_all(self.path.parent().unwrap_or(&self.path));
        let fresh_file = !self.path.exists();
        if self.records.len() >= MAX_RECORDS {
            // rewrite keeping the newest half
            let keep = &self.records[self.records.len() / 2..];
            let mut body = String::new();
            for r in keep {
                if let Ok(l) = serde_json::to_string(r) {
                    body.push_str(&l);
                    body.push('\n');
                }
            }
            body.push_str(&line);
            body.push('\n');
            let _ = fs::write(&self.path, body);
        } else if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(f, "{line}");
        }
        if fresh_file {
            self.prune();
        }
    }

    /// Delete session files older than 24 h (runs when a new session starts).
    fn prune(&self) {
        let Some(dir) = self.path.parent() else { return };
        let Ok(rd) = fs::read_dir(dir) else { return };
        let now = std::time::SystemTime::now();
        for e in rd.flatten() {
            if e.path() == self.path {
                continue;
            }
            if let Ok(md) = e.metadata()
                && let Ok(m) = md.modified()
                && now.duration_since(m).map(|d| d.as_secs() > MAX_AGE_SECS).unwrap_or(false)
            {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
