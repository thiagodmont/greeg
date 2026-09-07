//! `greeg stats`: opt-in, local-only records of what the Claude Code hook
//! rewrote and what every greeg run cost, plus a replay of the original
//! `rg`/`grep` commands for the counterfactual.
//!
//! Off by default: the records hold search patterns and paths. Enable with
//! `greeg stats enable` (writes `stats = true` to `~/.config/greeg/config.toml`)
//! or `GREEG_STATS=1` for one shell; `GREEG_STATS=0` overrides the file.
//!
//! Storage: `<cache dir>/stats/events.jsonl` (mode 0600), one JSON object per
//! line with `kind` = `hook` (the hook rewrote an rg/grep call) or `run` (a
//! greeg process finished). `replay.jsonl` holds `greeg stats replay` results.
//! A hook record and the run it produced share an `id`: blake3 of the working
//! directory and the rewritten argv, which the run recomputes from its own
//! argv, so the two join without passing state through the environment (an env
//! assignment in front of the command would break `Bash(greeg:*)` allow rules).
//! Recording is best effort: a failure never changes output or exit code.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// Claude Code truncates Bash tool output at 30 000 characters
/// (`BASH_MAX_OUTPUT_LENGTH`): rg output beyond that never reaches the model,
/// so savings are computed against the capped size unless `--cap` says otherwise.
pub const DEFAULT_CAP: usize = 30_000;
/// `events.jsonl` rotates to `events.1.jsonl` past this size (one generation kept).
const ROTATE_BYTES: u64 = 16 << 20;
/// A run pairs with the newest hook record of the same id at most this much older.
const PAIR_WINDOW_MS: u64 = 120_000;
/// Output kept in memory for the token estimate; beyond this only bytes are
/// counted and the estimate is scaled (`--budget 0` can print megabytes).
const CAPTURE_MAX: usize = 4 << 20;

// ---------------------------------------------------------------- config

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Config {
    pub enabled: Option<bool>,
    pub cap: Option<usize>,
}

/// `~/.config/greeg/config.toml` (the extra-languages directory lives next to it).
pub fn config_path() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("GREEG_CONFIG_DIR") {
        return Some(PathBuf::from(d).join("config.toml"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/greeg/config.toml"))
}

/// Minimal TOML subset: top-level `key = value` lines with `true`/`false`/integer values.
fn parse_config(s: &str) -> Config {
    let mut c = Config::default();
    for line in s.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            break; // only top-level keys are ours
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.split('#').next().unwrap_or("").trim();
        match k.trim() {
            "stats" => c.enabled = v.parse().ok(),
            "stats_cap" => c.cap = v.parse().ok(),
            _ => {}
        }
    }
    c
}

pub fn read_config() -> Config {
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| parse_config(&s))
        .unwrap_or_default()
}

/// Set or replace one top-level `key = value` line, keeping everything else.
/// A new key goes before the first `[table]` header so it stays top-level.
fn set_key(old: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut done = false;
    let mut in_table = false;
    for line in old.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if !done {
                lines.push(format!("{key} = {value}"));
                done = true;
            }
            in_table = true;
        }
        let is_key = !in_table
            && !t.starts_with('#')
            && t.split_once('=').is_some_and(|(k, _)| k.trim() == key);
        if is_key {
            if !done {
                lines.push(format!("{key} = {value}"));
                done = true;
            }
        } else {
            lines.push(line.to_string());
        }
    }
    if !done {
        lines.push(format!("{key} = {value}"));
    }
    lines.join("\n") + "\n"
}

fn write_config_key(key: &str, value: &str) -> Result<PathBuf> {
    let path = config_path().context("HOME is not set")?;
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, set_key(&old, key, value))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Where the decision came from, for `greeg stats status`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Source {
    Env,
    Config,
    Default,
}

fn env_on(v: &str) -> bool {
    !matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "off" | "no"
    )
}

fn decide() -> (bool, Source) {
    if let Ok(v) = std::env::var("GREEG_STATS") {
        return (env_on(&v), Source::Env);
    }
    match read_config().enabled {
        Some(b) => (b, Source::Config),
        None => (false, Source::Default),
    }
}

/// Is collection on? Decided once per process.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| decide().0)
}

/// The stats directory: `GREEG_STATS_DIR` or `<cache dir>/stats`.
pub fn dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("GREEG_STATS_DIR") {
        return Some(PathBuf::from(d));
    }
    greeg_index::cache_base().ok().map(|b| b.join("stats"))
}

// ---------------------------------------------------------------- records

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Event {
    Hook(HookEvent),
    Run(RunEvent),
}

/// The hook rewrote an `rg`/`grep` segment to greeg.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HookEvent {
    pub ts: u64,
    pub id: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The original segment as argv (`rg`, `-n`, `foo`, `src`).
    pub original: Vec<String>,
    /// The replacement as argv (`greeg`, `-n`, `foo`, `src`).
    pub rewritten: Vec<String>,
}

/// A greeg process finished.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RunEvent {
    pub ts: u64,
    pub id: String,
    pub cwd: String,
    /// The agent session the process ran in (`CLAUDE_CODE_SESSION_ID`, else `GREEG_SESSION`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub argv: Vec<String>,
    /// `search` or the verb name.
    pub verb: String,
    /// Process start to the end of output, as the caller experiences it.
    pub wall_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Bytes written to stdout and stderr.
    pub bytes: usize,
    /// Estimated o200k tokens of that output.
    pub tokens: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hits: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<usize>,
    pub exit: i32,
}

/// One replayed hook record: the original rg/grep and the greeg rewrite, run
/// under the same conditions, median of `runs` timed runs after one warm-up.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReplayEvent {
    pub ts: u64,
    pub id: String,
    pub runs: usize,
    pub rg_ms: f64,
    pub rg_bytes: usize,
    pub rg_tokens: usize,
    pub rg_exit: i32,
    pub greeg_ms: f64,
    pub greeg_bytes: usize,
    pub greeg_tokens: usize,
    pub greeg_exit: i32,
}

/// Join key shared by the hook record and the run it produced.
pub fn pair_id(cwd: &Path, argv: &[String]) -> String {
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut h = blake3::Hasher::new();
    h.update(cwd.to_string_lossy().as_bytes());
    for (i, w) in argv.iter().enumerate() {
        h.update(b"\0");
        // argv[0] is whatever the shell resolved: normalise to the program name
        if i == 0 {
            h.update(w.rsplit('/').next().unwrap_or(w).as_bytes());
        } else {
            h.update(w.as_bytes());
        }
    }
    h.finalize().to_hex()[..16].to_string()
}

/// One line, one `write` call: parallel greeg processes (Claude runs tool
/// calls concurrently) append to the same file and must not interleave.
fn append_in(dir: &Path, name: &str, line: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    if let Ok(m) = std::fs::metadata(&path)
        && m.len() > ROTATE_BYTES
    {
        let _ = std::fs::rename(&path, dir.join(name.replace(".jsonl", ".1.jsonl")));
    }
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(&path)?;
    let mut buf = Vec::with_capacity(line.len() + 1);
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    f.write_all(&buf)?;
    Ok(())
}

fn append(name: &str, line: &str) -> Result<()> {
    append_in(&dir().context("no cache directory")?, name, line)
}

/// Called by the hook after a rewrite. Best effort.
pub fn record_hook(cwd: &Path, session: Option<&str>, original: &[String], rewritten: &[String]) {
    if !enabled() {
        return;
    }
    let ev = Event::Hook(HookEvent {
        ts: greeg_index::now_ms(),
        id: pair_id(cwd, rewritten),
        cwd: cwd.to_string_lossy().into_owned(),
        session: session.map(str::to_string),
        original: original.to_vec(),
        rewritten: rewritten.to_vec(),
    });
    if let Ok(s) = serde_json::to_string(&ev) {
        let _ = append("events.jsonl", &s);
    }
}

/// The agent session a greeg process runs in: Claude Code exports
/// `CLAUDE_CODE_SESSION_ID` to its Bash tool (the same id its hooks receive);
/// `GREEG_SESSION` is the fallback for other agents.
pub fn agent_session() -> Option<String> {
    ["CLAUDE_CODE_SESSION_ID", "GREEG_SESSION"]
        .iter()
        .find_map(|k| std::env::var(k).ok())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------- capture

struct Capture {
    start: Instant,
    /// The first `CAPTURE_MAX` bytes.
    head: Vec<u8>,
    /// Everything, counted.
    bytes: usize,
}

static CAPTURE: Mutex<Option<Capture>> = Mutex::new(None);

/// A poisoned lock (a panic while another thread held it) must not take the
/// output path down with it: the data is still usable.
fn capture() -> MutexGuard<'static, Option<Capture>> {
    CAPTURE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Start of a greeg process: arms output capture when stats are on.
pub fn begin() {
    if enabled() {
        *capture() = Some(Capture {
            start: Instant::now(),
            head: Vec::new(),
            bytes: 0,
        });
    }
}

/// Bytes the model will read (stdout or stderr).
pub fn observe(b: &[u8]) {
    if let Some(c) = capture().as_mut() {
        c.bytes += b.len();
        let room = CAPTURE_MAX.saturating_sub(c.head.len());
        c.head.extend_from_slice(&b[..b.len().min(room)]);
    }
}

/// Token estimate for the whole output from the kept head.
fn estimate_tokens(head: &[u8], bytes: usize) -> usize {
    let t = greeg_query::tokens::rendered(head);
    if head.is_empty() || bytes <= head.len() {
        t
    } else {
        (t as f64 * bytes as f64 / head.len() as f64).round() as usize
    }
}

/// A writer that also feeds [`observe`].
pub struct Tee<W: Write>(pub W);

impl<W: Write> Write for Tee<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.0.write(buf)?;
        observe(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// What the run knows about itself beyond the captured output.
#[derive(Default)]
pub struct RunInfo {
    pub verb: &'static str,
    pub scan_ms: Option<f64>,
    pub shape_ms: Option<f64>,
    pub source: Option<String>,
    pub hits: Option<usize>,
    pub files: Option<usize>,
    pub exit: i32,
}

/// Time since the process started, in microseconds (macOS: from the kernel's
/// process start time, so it includes exec and dynamic loading).
pub fn since_start_us() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let r = unsafe {
            libc::proc_pidinfo(
                std::process::id() as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if r != size {
            return None;
        }
        let start = std::time::UNIX_EPOCH
            + std::time::Duration::new(info.pbi_start_tvsec, info.pbi_start_tvusec as u32 * 1000);
        return std::time::SystemTime::now()
            .duration_since(start)
            .ok()
            .map(|d| d.as_micros() as u64);
    }
    #[allow(unreachable_code)]
    None
}

/// End of a greeg process: write the run record. Best effort; call before any
/// `process::exit`. A no-op unless [`begin`] armed the capture.
pub fn record_run(info: RunInfo) {
    let Some(c) = capture().take() else {
        return;
    };
    let wall_ms = since_start_us()
        .map(|us| us as f64 / 1e3)
        .unwrap_or_else(|| c.start.elapsed().as_secs_f64() * 1e3);
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut argv: Vec<String> = std::env::args().collect();
    if let Some(a0) = argv.first_mut() {
        *a0 = "greeg".into();
    }
    let ev = Event::Run(RunEvent {
        ts: greeg_index::now_ms(),
        id: pair_id(&cwd, &argv),
        cwd: cwd.to_string_lossy().into_owned(),
        session: agent_session(),
        argv,
        verb: info.verb.to_string(),
        wall_ms,
        scan_ms: info.scan_ms,
        shape_ms: info.shape_ms,
        source: info.source,
        bytes: c.bytes,
        tokens: estimate_tokens(&c.head, c.bytes),
        hits: info.hits,
        files: info.files,
        exit: info.exit,
    });
    if let Ok(s) = serde_json::to_string(&ev) {
        let _ = append("events.jsonl", &s);
    }
}

/// A verb found nothing: record the run, then exit 1 like ripgrep.
pub fn exit_no_hits(verb: &'static str) -> ! {
    record_run(RunInfo {
        verb,
        hits: Some(0),
        exit: 1,
        ..Default::default()
    });
    greeg_query::indexed::flush_pending_build();
    std::process::exit(1)
}

// ---------------------------------------------------------------- reading

/// Lines that fail to parse (a torn write, an older schema) are skipped.
fn read_lines<T: for<'a> Deserialize<'a>>(path: &Path) -> Vec<T> {
    let Ok(s) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    s.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn load_events(dir: &Path) -> Vec<Event> {
    let mut v: Vec<Event> = read_lines(&dir.join("events.1.jsonl"));
    v.extend(read_lines::<Event>(&dir.join("events.jsonl")));
    v
}

fn load_replays(dir: &Path) -> Vec<ReplayEvent> {
    let mut v: Vec<ReplayEvent> = read_lines(&dir.join("replay.1.jsonl"));
    v.extend(read_lines::<ReplayEvent>(&dir.join("replay.jsonl")));
    v
}

/// `7d`, `12h`, `30m`, `2w`, `90s` → milliseconds.
pub fn parse_since(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.trim_end_matches(|c: char| c.is_ascii_alphabetic()).len());
    let n: f64 = num.parse().with_context(|| format!("bad duration {s:?}"))?;
    let mult = match unit {
        "s" => 1_000.0,
        "m" => 60_000.0,
        "h" => 3_600_000.0,
        "d" | "" => 86_400_000.0,
        "w" => 7.0 * 86_400_000.0,
        _ => anyhow::bail!("bad duration unit in {s:?} (use s, m, h, d, w)"),
    };
    Ok((n * mult) as u64)
}

/// Filters shared by the report and the replay.
#[derive(Default)]
pub struct Filter {
    pub since_ms: Option<u64>,
    pub repo: Option<PathBuf>,
    /// An agent session id, or a prefix of one.
    pub session: Option<String>,
}

impl Filter {
    fn cutoff(&self) -> u64 {
        self.since_ms
            .map(|d| greeg_index::now_ms().saturating_sub(d))
            .unwrap_or(0)
    }
    fn repo_canon(&self) -> Option<PathBuf> {
        self.repo
            .as_ref()
            .map(|r| std::fs::canonicalize(r).unwrap_or_else(|_| r.clone()))
    }
    fn keep(&self, ts: u64, cwd: &str, cutoff: u64, repo: &Option<PathBuf>) -> bool {
        if ts < cutoff {
            return false;
        }
        match repo {
            Some(r) => {
                let c = std::fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
                c.starts_with(r)
            }
            None => true,
        }
    }
}

/// Events after filtering, with runs paired to their hook record.
struct Loaded {
    hooks: Vec<HookEvent>,
    runs: Vec<RunEvent>,
    /// `runs[i]` came from `hooks[pair[i]]`.
    pair: Vec<Option<usize>>,
    replays: HashMap<String, ReplayEvent>,
    /// Events on disk before filtering.
    total: usize,
}

fn load(dir: &Path, f: &Filter) -> Loaded {
    let cutoff = f.cutoff();
    let repo = f.repo_canon();
    let mut hooks = Vec::new();
    let mut runs = Vec::new();
    let all = load_events(dir);
    let total = all.len();
    for e in all {
        match e {
            Event::Hook(h) if f.keep(h.ts, &h.cwd, cutoff, &repo) => hooks.push(h),
            Event::Run(r) if f.keep(r.ts, &r.cwd, cutoff, &repo) => runs.push(r),
            _ => {}
        }
    }
    let mut pair = pair_runs(&hooks, &runs);
    if let Some(sid) = f.session.as_deref() {
        // a run belongs to its own session or, when rewritten, to its hook's
        let keep_run: Vec<bool> = runs
            .iter()
            .enumerate()
            .map(|(i, r)| {
                r.session
                    .as_deref()
                    .or_else(|| pair[i].and_then(|hi| hooks[hi].session.as_deref()))
                    .is_some_and(|s| s.starts_with(sid))
            })
            .collect();
        let mut keep = keep_run.iter();
        runs.retain(|_| *keep.next().unwrap_or(&false));
        hooks.retain(|h| h.session.as_deref().is_some_and(|s| s.starts_with(sid)));
        pair = pair_runs(&hooks, &runs);
    }
    let replays = load_replays(dir)
        .into_iter()
        .map(|r| (r.id.clone(), r))
        .collect();
    Loaded {
        hooks,
        runs,
        pair,
        replays,
        total,
    }
}

/// Each run takes the newest unused hook record with its id from the pairing
/// window before it (two identical rg calls in a row give two hooks, two runs).
fn pair_runs(hooks: &[HookEvent], runs: &[RunEvent]) -> Vec<Option<usize>> {
    let mut by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    for (hi, h) in hooks.iter().enumerate() {
        by_id.entry(h.id.as_str()).or_default().push(hi);
    }
    for v in by_id.values_mut() {
        v.sort_by_key(|&hi| hooks[hi].ts);
    }
    let mut used = vec![false; hooks.len()];
    let mut order: Vec<usize> = (0..runs.len()).collect();
    order.sort_by_key(|&i| runs[i].ts);
    let mut pair = vec![None; runs.len()];
    for ri in order {
        let r = &runs[ri];
        let Some(cands) = by_id.get(r.id.as_str()) else {
            continue;
        };
        // candidates are sorted by ts ascending: walk them newest first
        let best =
            cands.iter().rev().copied().find(|&hi| {
                !used[hi] && hooks[hi].ts <= r.ts && r.ts - hooks[hi].ts <= PAIR_WINDOW_MS
            });
        if let Some(hi) = best {
            used[hi] = true;
            pair[ri] = Some(hi);
        }
    }
    pair
}

// ---------------------------------------------------------------- numbers

#[derive(Serialize, Debug, Clone, Default)]
pub struct Dist {
    pub n: usize,
    pub avg: f64,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
    pub total: f64,
}

/// Nearest-rank percentile on a sorted slice.
fn pct(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

impl Dist {
    pub fn of(mut v: Vec<f64>) -> Dist {
        if v.is_empty() {
            return Dist::default();
        }
        v.sort_by(|a, b| a.total_cmp(b));
        let total: f64 = v.iter().sum();
        Dist {
            n: v.len(),
            avg: total / v.len() as f64,
            min: v[0],
            p50: pct(&v, 0.50),
            p95: pct(&v, 0.95),
            p99: pct(&v, 0.99),
            max: v[v.len() - 1],
            total,
        }
    }
}

/// rg tokens the model would actually read: the estimate scaled to the cap
/// (tokens are close to proportional to bytes for grep-shaped output).
fn capped_tokens(r: &ReplayEvent, cap: usize) -> f64 {
    if cap == 0 || r.rg_bytes <= cap {
        r.rg_tokens as f64
    } else {
        r.rg_tokens as f64 * cap as f64 / r.rg_bytes as f64
    }
}

fn thousands(n: f64) -> String {
    let s = format!("{:.0}", n.round());
    let (neg, digits) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, s.as_str()),
    };
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if neg { format!("-{out}") } else { out }
}

fn ms(v: f64) -> String {
    if v.abs() < 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.0}")
    }
}

fn date(ts: u64) -> String {
    // civil date from unix days (Howard Hinnant's algorithm), UTC
    let days = (ts / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Oldest and newest timestamp among the loaded records.
fn span(l: &Loaded) -> (u64, u64) {
    l.runs
        .iter()
        .map(|r| r.ts)
        .chain(l.hooks.iter().map(|h| h.ts))
        .fold((u64::MAX, 0), |(a, b), t| (a.min(t), b.max(t)))
}

// ---------------------------------------------------------------- report

pub struct ReportOpts {
    pub filter: Filter,
    pub cap: Option<usize>,
    pub json: bool,
    pub verbose: bool,
}

pub fn effective_cap(flag: Option<usize>) -> usize {
    flag.or(read_config().cap).unwrap_or(DEFAULT_CAP)
}

pub fn report(o: &ReportOpts) -> Result<()> {
    let dir = dir().context("no cache directory")?;
    let mut w = std::io::stdout().lock();
    report_to(&mut w, &dir, o, decide(), &crate::hook::other_bash_hooks())
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|w| crate::hook::quote(w))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `4.39 s`, `42 ms`, `2.9 ms`; the sign is kept.
fn human_ms(v: f64) -> String {
    let a = v.abs();
    if a >= 1000.0 {
        format!("{:.2} s", v / 1000.0)
    } else if a >= 10.0 {
        format!("{v:.0} ms")
    } else {
        format!("{v:.1} ms")
    }
}

fn pct_of(part: f64, whole: f64) -> f64 {
    if whole > 0.0 {
        part / whole * 100.0
    } else {
        0.0
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!(
        "{} {}",
        thousands(n as f64),
        if n == 1 { one } else { many }
    )
}

/// At most `n` characters, `…` when cut.
fn clip(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// A hook command with the path stripped from its program: `git-ai checkpoint …`.
fn program_name(cmd: &str) -> String {
    let (head, tail) = cmd.split_once(' ').unwrap_or((cmd, ""));
    let head = head.rsplit('/').next().unwrap_or(head);
    if tail.is_empty() {
        head.to_string()
    } else {
        format!("{head} {tail}")
    }
}

/// `$HOME` as `~`.
fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && path.starts_with(&h) => format!("~{}", &path[h.len()..]),
        _ => path.to_string(),
    }
}

/// rg/grep found matches and greeg found none: a retry for the agent, not a saving.
fn is_miss(rp: &ReplayEvent) -> bool {
    rp.rg_exit == 0 && rp.greeg_exit != 0
}

/// One replayed query, as `--verbose` lists it.
#[derive(Serialize, Debug, Clone)]
struct PerQuery {
    id: String,
    /// Paired runs that used it.
    n: usize,
    saved_tokens: f64,
    saved_ms: f64,
    rg_tokens: f64,
    greeg_tokens: f64,
    rg_ms: f64,
    greeg_ms: f64,
    missed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
}

/// The largest wins on one axis: how much of the saving a few queries hold.
#[derive(Serialize, Debug, Clone, Default)]
struct Top {
    queries: usize,
    runs: usize,
    saved: f64,
}

/// Up to three queries with the largest positive saving under `key`, weighted
/// by the runs that used them.
fn top_of(queries: &[PerQuery], key: fn(&PerQuery) -> f64) -> Top {
    let mut v: Vec<&PerQuery> = queries.iter().filter(|q| key(q) > 0.0).collect();
    v.sort_by(|a, b| key(b).total_cmp(&key(a)).then_with(|| a.id.cmp(&b.id)));
    v.iter().take(3).fold(Top::default(), |t, q| Top {
        queries: t.queries + 1,
        runs: t.runs + q.n,
        saved: t.saved + q.n as f64 * key(q),
    })
}

/// Runs recorded under one working directory.
#[derive(Serialize, Debug, Clone, Default)]
struct DirRuns {
    dir: String,
    rewritten: usize,
    direct: usize,
    verbs: usize,
    tokens: f64,
}

impl DirRuns {
    fn runs(&self) -> usize {
        self.rewritten + self.direct + self.verbs
    }
}

/// One agent session: what greeg did in it and what its replayed rewrites saved.
#[derive(Serialize, Debug, Clone)]
struct SessionRow {
    session: String,
    started: u64,
    /// The working directory most of its records share.
    dir: String,
    rewritten: usize,
    replayed: usize,
    saved_tokens: f64,
    saved_ms: f64,
    missed: usize,
    /// Hook rewrites no greeg run followed.
    lost: usize,
    direct: usize,
    verbs: usize,
    /// Tokens of greeg output the agent read.
    greeg_tokens: f64,
}

impl SessionRow {
    fn new(session: &str) -> SessionRow {
        SessionRow {
            session: session.to_string(),
            started: u64::MAX,
            dir: String::new(),
            rewritten: 0,
            replayed: 0,
            saved_tokens: 0.0,
            saved_ms: 0.0,
            missed: 0,
            lost: 0,
            direct: 0,
            verbs: 0,
            greeg_tokens: 0.0,
        }
    }
}

/// Everything the report prints, computed once so text and JSON agree.
struct Summary {
    first: u64,
    last: u64,
    cap: usize,
    n_rewritten: usize,
    n_direct: usize,
    n_verbs: usize,
    n_hooks: usize,
    /// Hook records that no greeg run followed.
    hooks_without_run: usize,
    lat_rewritten: Dist,
    lat_direct: Dist,
    lat_verbs: Dist,
    tk_rewritten: Dist,
    tk_direct: Dist,
    tk_verbs: Dist,
    /// Paired runs with a replay record (a query run twice counts twice).
    n_replayed: usize,
    unreplayed: usize,
    lat_rg: Dist,
    lat_gg: Dist,
    /// rg minus greeg, per replayed run.
    lat_saved: Dist,
    tk_rg_raw: Dist,
    tk_rg_cap: Dist,
    tk_gg: Dist,
    tk_saved: Dist,
    tok_smaller: usize,
    tok_larger: usize,
    tok_equal: usize,
    ms_faster: usize,
    ms_slower: usize,
    missed: usize,
    /// Unique replayed queries, largest token saving first.
    queries: Vec<PerQuery>,
    top_tokens: Top,
    top_ms: Top,
    /// Runs per working directory, most runs first.
    dirs: Vec<DirRuns>,
    /// Agent sessions, newest first.
    sessions: Vec<SessionRow>,
    /// Runs with no session id of their own or their hook's.
    runs_without_session: usize,
}

fn summarize(l: &Loaded, cap: usize) -> Summary {
    let (first, last) = span(l);
    // live: 0 rewritten searches, 1 direct searches, 2 verbs
    let mut lat = [Vec::new(), Vec::new(), Vec::new()];
    let mut tok = [Vec::new(), Vec::new(), Vec::new()];
    let mut by_dir: HashMap<&str, DirRuns> = HashMap::new();
    for (i, r) in l.runs.iter().enumerate() {
        let class = if r.verb != "search" {
            2
        } else if l.pair[i].is_some() {
            0
        } else {
            1
        };
        lat[class].push(r.wall_ms);
        tok[class].push(r.tokens as f64);
        let e = by_dir.entry(r.cwd.as_str()).or_default();
        match class {
            0 => e.rewritten += 1,
            1 => e.direct += 1,
            _ => e.verbs += 1,
        }
        e.tokens += r.tokens as f64;
    }
    let mut dirs: Vec<DirRuns> = by_dir
        .into_iter()
        .map(|(d, e)| DirRuns { dir: tilde(d), ..e })
        .collect();
    dirs.sort_by(|x, y| y.runs().cmp(&x.runs()).then_with(|| x.dir.cmp(&y.dir)));
    let (n_rewritten, n_direct, n_verbs) = (lat[0].len(), lat[1].len(), lat[2].len());
    let [lat_rewritten, lat_direct, lat_verbs] = lat.map(Dist::of);
    let [tk_rewritten, tk_direct, tk_verbs] = tok.map(Dist::of);

    // replay, weighted by the paired runs that used each query
    let (mut rg_ms, mut gg_ms, mut d_ms) = (Vec::new(), Vec::new(), Vec::new());
    let (mut rg_raw, mut rg_cap, mut gg_tok, mut d_tok) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut per_id: HashMap<&str, usize> = HashMap::new();
    let mut unreplayed = 0usize;
    let (mut tok_smaller, mut tok_larger, mut tok_equal) = (0usize, 0usize, 0usize);
    let (mut ms_faster, mut ms_slower, mut missed) = (0usize, 0usize, 0usize);
    for (i, r) in l.runs.iter().enumerate() {
        if l.pair[i].is_none() {
            continue;
        }
        let Some(rp) = l.replays.get(&r.id) else {
            unreplayed += 1;
            continue;
        };
        *per_id.entry(rp.id.as_str()).or_default() += 1;
        let ct = capped_tokens(rp, cap);
        let (dt, dm) = (ct - rp.greeg_tokens as f64, rp.rg_ms - rp.greeg_ms);
        rg_ms.push(rp.rg_ms);
        gg_ms.push(rp.greeg_ms);
        d_ms.push(dm);
        rg_raw.push(rp.rg_tokens as f64);
        rg_cap.push(ct);
        gg_tok.push(rp.greeg_tokens as f64);
        d_tok.push(dt);
        match dt.partial_cmp(&0.0) {
            Some(Ordering::Greater) => tok_smaller += 1,
            Some(Ordering::Less) => tok_larger += 1,
            _ => tok_equal += 1,
        }
        if dm > 0.0 {
            ms_faster += 1;
        } else {
            ms_slower += 1;
        }
        if is_miss(rp) {
            missed += 1;
        }
    }
    let n_replayed = rg_ms.len();
    let mut queries: Vec<PerQuery> = per_id
        .iter()
        .map(|(id, &n)| {
            let rp = &l.replays[*id];
            let ct = capped_tokens(rp, cap);
            PerQuery {
                id: id.to_string(),
                n,
                saved_tokens: ct - rp.greeg_tokens as f64,
                saved_ms: rp.rg_ms - rp.greeg_ms,
                rg_tokens: ct,
                greeg_tokens: rp.greeg_tokens as f64,
                rg_ms: rp.rg_ms,
                greeg_ms: rp.greeg_ms,
                missed: is_miss(rp),
                command: l
                    .hooks
                    .iter()
                    .find(|h| h.id == *id)
                    .map(|h| shell_join(&h.original)),
            }
        })
        .collect();
    queries.sort_by(|a, b| {
        b.saved_tokens
            .total_cmp(&a.saved_tokens)
            .then_with(|| a.id.cmp(&b.id))
    });
    let top_tokens = top_of(&queries, |q| q.saved_tokens);
    let top_ms = top_of(&queries, |q| q.saved_ms);

    // sessions: a run belongs to its own session or, when rewritten, to its hook's
    let mut by_session: HashMap<&str, (SessionRow, HashMap<&str, usize>)> = HashMap::new();
    let mut runs_without_session = 0usize;
    for (i, r) in l.runs.iter().enumerate() {
        let sid = r
            .session
            .as_deref()
            .or_else(|| l.pair[i].and_then(|hi| l.hooks[hi].session.as_deref()));
        let Some(sid) = sid else {
            runs_without_session += 1;
            continue;
        };
        let (row, dirs) = by_session
            .entry(sid)
            .or_insert_with(|| (SessionRow::new(sid), HashMap::new()));
        row.started = row.started.min(r.ts);
        *dirs.entry(r.cwd.as_str()).or_default() += 1;
        row.greeg_tokens += r.tokens as f64;
        if r.verb != "search" {
            row.verbs += 1;
        } else if l.pair[i].is_some() {
            row.rewritten += 1;
            if let Some(rp) = l.replays.get(&r.id) {
                row.replayed += 1;
                row.saved_tokens += capped_tokens(rp, cap) - rp.greeg_tokens as f64;
                row.saved_ms += rp.rg_ms - rp.greeg_ms;
                if is_miss(rp) {
                    row.missed += 1;
                }
            }
        } else {
            row.direct += 1;
        }
    }
    let mut paired_hook = vec![false; l.hooks.len()];
    for hi in l.pair.iter().flatten() {
        paired_hook[*hi] = true;
    }
    for (hi, h) in l.hooks.iter().enumerate() {
        let Some(sid) = h.session.as_deref() else {
            continue;
        };
        let (row, dirs) = by_session
            .entry(sid)
            .or_insert_with(|| (SessionRow::new(sid), HashMap::new()));
        row.started = row.started.min(h.ts);
        *dirs.entry(h.cwd.as_str()).or_default() += 1;
        if !paired_hook[hi] {
            row.lost += 1;
        }
    }
    let mut sessions: Vec<SessionRow> = by_session
        .into_values()
        .map(|(mut row, dirs)| {
            row.dir = dirs
                .iter()
                .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
                .map(|(d, _)| tilde(d))
                .unwrap_or_default();
            row
        })
        .collect();
    sessions.sort_by(|a, b| {
        b.started
            .cmp(&a.started)
            .then_with(|| a.session.cmp(&b.session))
    });
    Summary {
        first,
        last,
        cap,
        n_rewritten,
        n_direct,
        n_verbs,
        n_hooks: l.hooks.len(),
        hooks_without_run: l.hooks.len() - l.pair.iter().flatten().count(),
        lat_rewritten,
        lat_direct,
        lat_verbs,
        tk_rewritten,
        tk_direct,
        tk_verbs,
        n_replayed,
        unreplayed,
        lat_rg: Dist::of(rg_ms),
        lat_gg: Dist::of(gg_ms),
        lat_saved: Dist::of(d_ms),
        tk_rg_raw: Dist::of(rg_raw),
        tk_rg_cap: Dist::of(rg_cap),
        tk_gg: Dist::of(gg_tok),
        tk_saved: Dist::of(d_tok),
        tok_smaller,
        tok_larger,
        tok_equal,
        ms_faster,
        ms_slower,
        missed,
        queries,
        top_tokens,
        top_ms,
        dirs,
        sessions,
        runs_without_session,
    }
}

/// A table row: label, distribution, cell formatter.
type DistRow<'a> = (&'a str, &'a Dist, fn(f64) -> String);

/// n/avg/min/p50/p95/p99/max/total per row; columns widen to the widest cell.
fn write_dist_table(w: &mut impl Write, unit: &str, rows: &[DistRow<'_>]) -> Result<()> {
    const HEAD: [&str; 8] = ["n", "avg", "min", "p50", "p95", "p99", "max", "total"];
    let rows: Vec<(&str, Vec<String>)> = rows
        .iter()
        .filter(|(_, d, _)| d.n > 0)
        .map(|(label, d, f)| {
            let cells = vec![
                d.n.to_string(),
                f(d.avg),
                f(d.min),
                f(d.p50),
                f(d.p95),
                f(d.p99),
                f(d.max),
                thousands(d.total),
            ];
            (*label, cells)
        })
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    let mut width = [6usize, 8, 8, 8, 8, 8, 8, 12];
    for (_, cells) in &rows {
        for (wd, c) in width.iter_mut().zip(cells) {
            *wd = (*wd).max(c.chars().count() + 2);
        }
    }
    let label_w = rows
        .iter()
        .map(|(l, _)| l.chars().count() + 2)
        .chain([24, unit.chars().count() + 2])
        .max()
        .unwrap_or(24);
    write!(w, "{unit:<label_w$}")?;
    for (h, wd) in HEAD.iter().zip(width) {
        write!(w, "{h:>wd$}")?;
    }
    writeln!(w)?;
    for (label, cells) in &rows {
        write!(w, "{label:<label_w$}")?;
        for (c, wd) in cells.iter().zip(width) {
            write!(w, "{c:>wd$}")?;
        }
        writeln!(w)?;
    }
    Ok(())
}

/// `largest token win holds` / `3 largest time wins hold`.
fn largest(t: &Top, axis: &str) -> String {
    if t.queries == 1 {
        format!("largest {axis} win holds")
    } else {
        format!("{} largest {axis} wins hold", t.queries)
    }
}

fn headline_row(
    w: &mut impl Write,
    label: &str,
    rg: &str,
    greeg: &str,
    saved: &str,
    note: &str,
) -> Result<()> {
    let line = format!("  {label:<22}{rg:>13}{greeg:>13}{saved:>13}  {note}");
    writeln!(w, "{}", line.trim_end())?;
    Ok(())
}

fn report_to(
    w: &mut impl Write,
    dir: &Path,
    o: &ReportOpts,
    on: (bool, Source),
    other_hooks: &[String],
) -> Result<()> {
    let l = load(dir, &o.filter);
    let cap = effective_cap(o.cap);
    if l.runs.is_empty() && l.hooks.is_empty() {
        if let Some(sid) = o.filter.session.as_deref().filter(|_| l.total > 0) {
            writeln!(
                w,
                "greeg stats: no records for session {sid} ({} on disk); the hook records the session of every rewrite, greeg runs record CLAUDE_CODE_SESSION_ID since 0.4",
                l.total
            )?;
        } else if l.total > 0 {
            writeln!(
                w,
                "greeg stats: no records match the filter ({} on disk)",
                l.total
            )?;
        } else if !on.0 {
            writeln!(
                w,
                "greeg stats: collection is off (default). `greeg stats enable` turns it on; records stay on this machine under {}",
                dir.display()
            )?;
        } else {
            writeln!(
                w,
                "greeg stats: no records yet under {}; run some greeg queries first",
                dir.display()
            )?;
        }
        return Ok(());
    }
    let s = summarize(&l, cap);
    if o.json {
        return write_json(w, &s, o.verbose);
    }

    let scope = match o.filter.session.as_deref() {
        Some(sid) => format!("session {sid} · "),
        None => String::new(),
    };
    let tail = match o.filter.session {
        Some(_) => String::new(),
        None => format!(" · {}", plural(s.sessions.len(), "session", "sessions")),
    };
    writeln!(
        w,
        "greeg stats · {scope}{} → {} · {} rewritten from rg/grep by the hook ({} replayed) · {} · {} (no rg counterfactual){tail}",
        date(s.first),
        date(s.last),
        plural(s.n_rewritten, "query", "queries"),
        s.n_replayed,
        plural(s.n_direct, "direct search", "direct searches"),
        plural(s.n_verbs, "symbol verb", "symbol verbs")
    )?;
    writeln!(w)?;
    if s.n_replayed > 0 {
        writeln!(
            w,
            "savings vs rg/grep · {} replayed {} · same machine and tree · rg output capped at {} bytes",
            s.n_replayed,
            if s.n_replayed == 1 {
                "query"
            } else {
                "queries"
            },
            thousands(s.cap as f64)
        )?;
        headline_row(w, "", "rg/grep", "greeg", "saved", "")?;
        headline_row(
            w,
            "tokens, total",
            &thousands(s.tk_rg_cap.total),
            &thousands(s.tk_gg.total),
            &thousands(s.tk_saved.total),
            &format!("{:.0}%", pct_of(s.tk_saved.total, s.tk_rg_cap.total)),
        )?;
        let mut split = format!(
            "greeg smaller on {}, larger on {}",
            plural(s.tok_smaller, "query", "queries"),
            s.tok_larger
        );
        if s.tok_equal > 0 {
            split.push_str(&format!(", equal on {}", s.tok_equal));
        }
        headline_row(
            w,
            "tokens, typical query",
            &thousands(s.tk_rg_cap.p50),
            &thousands(s.tk_gg.p50),
            &thousands(s.tk_saved.p50),
            &split,
        )?;
        headline_row(
            w,
            "time, total",
            &human_ms(s.lat_rg.total),
            &human_ms(s.lat_gg.total),
            &human_ms(s.lat_saved.total),
            &format!("{:.0}%", pct_of(s.lat_saved.total, s.lat_rg.total)),
        )?;
        headline_row(
            w,
            "time, typical query",
            &human_ms(s.lat_rg.p50),
            &human_ms(s.lat_gg.p50),
            &human_ms(s.lat_saved.p50),
            &format!(
                "greeg faster on {}, slower on {}",
                plural(s.ms_faster, "query", "queries"),
                s.ms_slower
            ),
        )?;
        writeln!(
            w,
            "  typical = median; on those rows `saved` is the median of the per-query differences"
        )?;
        // worth a line only when a few queries hold more than everything else combined
        let others = s.n_replayed - s.top_tokens.runs;
        let rest = s.tk_saved.total - s.top_tokens.saved;
        if s.top_tokens.queries > 0 && others > 0 && s.top_tokens.saved > rest.abs() {
            writeln!(
                w,
                "  the {} {} tokens; the other {} net {} (--verbose lists every query)",
                largest(&s.top_tokens, "token"),
                thousands(s.top_tokens.saved),
                plural(others, "query", "queries"),
                thousands(s.tk_saved.total - s.top_tokens.saved)
            )?;
        }
        let others = s.n_replayed - s.top_ms.runs;
        let rest = s.lat_saved.total - s.top_ms.saved;
        if s.top_ms.queries > 0 && others > 0 && s.top_ms.saved > rest.abs() {
            writeln!(
                w,
                "  the {} {}; the other {} net {}",
                largest(&s.top_ms, "time"),
                human_ms(s.top_ms.saved),
                plural(others, "query", "queries"),
                human_ms(s.lat_saved.total - s.top_ms.saved)
            )?;
        }
        if s.missed > 0 {
            writeln!(
                w,
                "  {} found nothing where rg/grep had matches: a retry for the agent, not a saving (--verbose marks them with !)",
                plural(s.missed, "query", "queries")
            )?;
        }
        if s.n_replayed < 100 {
            writeln!(
                w,
                "  note: p95/p99 need a few hundred queries before they mean much"
            )?;
        }
    }
    if s.unreplayed > 0 {
        writeln!(
            w,
            "  {} no rg counterfactual yet: `greeg stats replay` runs the original commands on a quiet machine",
            plural(
                s.unreplayed,
                "rewritten query has",
                "rewritten queries have"
            )
        )?;
    }
    if s.hooks_without_run > 0 {
        let cause = if other_hooks.is_empty() {
            "the call was cancelled, or another PreToolUse hook rewrote it and finished last"
                .to_string()
        } else {
            let list = other_hooks
                .iter()
                .take(3)
                .map(|c| format!("`{}`", clip(&program_name(c), 40)))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "another PreToolUse hook rewrote the same call and finished last (candidates: {list}), or the call was cancelled"
            )
        };
        writeln!(
            w,
            "  {} not followed by a greeg run: {cause}",
            plural(
                s.hooks_without_run,
                "hook rewrite was",
                "hook rewrites were"
            )
        )?;
    }
    writeln!(w)?;
    write_dist_table(
        w,
        "latency (ms)",
        &[
            ("greeg, rewritten", &s.lat_rewritten, ms),
            ("greeg, direct search", &s.lat_direct, ms),
            ("greeg, verbs", &s.lat_verbs, ms),
            ("greeg (replayed)", &s.lat_gg, ms),
            ("rg/grep (replayed)", &s.lat_rg, ms),
            ("saved per query", &s.lat_saved, ms),
        ],
    )?;
    writeln!(w)?;
    let capped = format!("rg/grep capped {}", thousands(s.cap as f64));
    write_dist_table(
        w,
        "tokens (est. o200k)",
        &[
            ("greeg, rewritten", &s.tk_rewritten, thousands),
            ("greeg, direct search", &s.tk_direct, thousands),
            ("greeg, verbs", &s.tk_verbs, thousands),
            ("greeg (replayed)", &s.tk_gg, thousands),
            ("rg/grep raw (replayed)", &s.tk_rg_raw, thousands),
            (&capped, &s.tk_rg_cap, thousands),
            ("saved per query", &s.tk_saved, thousands),
        ],
    )?;
    if o.verbose {
        if !s.queries.is_empty() {
            writeln!(w)?;
            writeln!(
                w,
                "replayed queries, largest token saving first (! = rg/grep found matches, greeg found none)"
            )?;
            writeln!(
                w,
                "{:>4}  {:>9} {:>9} {:>8} {:>9} {:>8} {:>9}  command",
                "n", "saved tok", "saved ms", "rg tok", "greeg tok", "rg ms", "greeg ms"
            )?;
            for q in &s.queries {
                writeln!(
                    w,
                    "{:>4}{} {:>9} {:>9} {:>8} {:>9} {:>8} {:>9}  {}",
                    q.n,
                    if q.missed { "!" } else { " " },
                    thousands(q.saved_tokens),
                    ms(q.saved_ms),
                    thousands(q.rg_tokens),
                    thousands(q.greeg_tokens),
                    ms(q.rg_ms),
                    ms(q.greeg_ms),
                    q.command.as_deref().unwrap_or("")
                )?;
            }
        }
        writeln!(w)?;
        writeln!(w, "runs by directory")?;
        writeln!(
            w,
            "{:>6} {:>9} {:>7} {:>6} {:>13}  directory",
            "runs", "rewritten", "direct", "verbs", "tokens"
        )?;
        for d in s.dirs.iter().take(12) {
            writeln!(
                w,
                "{:>6} {:>9} {:>7} {:>6} {:>13}  {}",
                d.runs(),
                d.rewritten,
                d.direct,
                d.verbs,
                thousands(d.tokens),
                d.dir
            )?;
        }
    }
    Ok(())
}

fn write_json(w: &mut impl Write, s: &Summary, verbose: bool) -> Result<()> {
    let mut queries = s.queries.clone();
    if !verbose {
        for q in &mut queries {
            q.command = None;
        }
    }
    let out = serde_json::json!({
        "from": date(s.first), "to": date(s.last), "cap": s.cap,
        "runs": {"rewritten": s.n_rewritten, "direct": s.n_direct, "verbs": s.n_verbs},
        "hooks": s.n_hooks, "hooks_without_run": s.hooks_without_run,
        "latency_ms": {"rewritten": s.lat_rewritten, "direct": s.lat_direct, "verbs": s.lat_verbs,
                        "rg_replay": s.lat_rg, "greeg_replay": s.lat_gg, "saved_per_query": s.lat_saved},
        "tokens": {"rewritten": s.tk_rewritten, "direct": s.tk_direct, "verbs": s.tk_verbs,
                   "rg_raw_replay": s.tk_rg_raw, "rg_capped_replay": s.tk_rg_cap, "greeg_replay": s.tk_gg,
                   "saved_per_query": s.tk_saved},
        "replayed": s.n_replayed, "unreplayed": s.unreplayed,
        "savings": {
            "tokens": {"rg": s.tk_rg_cap.total, "greeg": s.tk_gg.total, "saved": s.tk_saved.total,
                       "pct": pct_of(s.tk_saved.total, s.tk_rg_cap.total), "median_per_query": s.tk_saved.p50,
                       "greeg_smaller": s.tok_smaller, "greeg_larger": s.tok_larger, "equal": s.tok_equal},
            "time_ms": {"rg": s.lat_rg.total, "greeg": s.lat_gg.total, "saved": s.lat_saved.total,
                        "pct": pct_of(s.lat_saved.total, s.lat_rg.total), "median_per_query": s.lat_saved.p50,
                        "greeg_faster": s.ms_faster, "greeg_slower": s.ms_slower},
            "top": {"tokens": s.top_tokens, "time_ms": s.top_ms}, "missed": s.missed,
        },
        "queries": queries,
        "dirs": s.dirs,
        "sessions": s.sessions, "runs_without_session": s.runs_without_session,
    });
    writeln!(w, "{}", serde_json::to_string_pretty(&out)?)?;
    Ok(())
}

/// `2026-09-07 11:02`, UTC.
fn datetime(ts: u64) -> String {
    let secs = ts / 1000 % 86_400;
    format!("{} {:02}:{:02}", date(ts), secs / 3600, secs % 3600 / 60)
}

/// `greeg stats sessions`: what each agent session saved, newest first.
pub fn sessions(o: &ReportOpts) -> Result<()> {
    let dir = dir().context("no cache directory")?;
    let mut w = std::io::stdout().lock();
    sessions_to(&mut w, &dir, o, agent_session().as_deref())
}

fn sessions_to(
    w: &mut impl Write,
    dir: &Path,
    o: &ReportOpts,
    current: Option<&str>,
) -> Result<()> {
    let l = load(dir, &o.filter);
    let s = summarize(&l, effective_cap(o.cap));
    if o.json {
        let out = serde_json::json!({
            "cap": s.cap,
            "sessions": s.sessions,
            "runs_without_session": s.runs_without_session,
        });
        writeln!(w, "{}", serde_json::to_string_pretty(&out)?)?;
        return Ok(());
    }
    if s.sessions.is_empty() {
        writeln!(
            w,
            "greeg stats: no session ids in the records ({} on disk): the hook records the Claude Code session, greeg runs record CLAUDE_CODE_SESSION_ID (or GREEG_SESSION) since 0.4",
            l.total
        )?;
        return Ok(());
    }
    writeln!(
        w,
        "sessions · newest first · saved = replayed rewrites vs rg/grep capped at {} bytes · lost = hook rewrites no greeg run followed{}",
        thousands(s.cap as f64),
        if current.is_some() {
            " · * = this session"
        } else {
            ""
        }
    )?;
    writeln!(
        w,
        "{:>16}  {:<8}  {:>9} {:>8} {:>12} {:>10} {:>5} {:>6} {:>5}  directory",
        "started (UTC)",
        "session",
        "rewritten",
        "replayed",
        "saved tokens",
        "saved time",
        "lost",
        "direct",
        "verbs"
    )?;
    for r in &s.sessions {
        let mark = if current == Some(r.session.as_str()) {
            "*"
        } else {
            " "
        };
        writeln!(
            w,
            "{:>16} {mark}{:<8}  {:>9} {:>8} {:>12} {:>10} {:>5} {:>6} {:>5}  {}",
            datetime(r.started),
            r.session.chars().take(8).collect::<String>(),
            r.rewritten,
            r.replayed,
            thousands(r.saved_tokens),
            human_ms(r.saved_ms),
            r.lost,
            r.direct,
            r.verbs,
            r.dir
        )?;
    }
    if s.runs_without_session > 0 {
        writeln!(
            w,
            "  {} no session id (recorded by an older greeg, or outside an agent) and {} left out",
            plural(s.runs_without_session, "run has", "runs have"),
            if s.runs_without_session == 1 {
                "is"
            } else {
                "are"
            }
        )?;
    }
    writeln!(
        w,
        "  `greeg stats --session-id ID` (a prefix will do; `current` inside an agent) reports one session in full"
    )?;
    Ok(())
}

// ---------------------------------------------------------------- enable / status / clear

pub fn enable(cap: Option<usize>) -> Result<()> {
    let p = write_config_key("stats", "true")?;
    if let Some(c) = cap {
        write_config_key("stats_cap", &c.to_string())?;
    }
    let d = dir().context("no cache directory")?;
    println!(
        "greeg stats: enabled in {}\nrecords: {} (this machine only, mode 0600; they hold search patterns and paths; `greeg stats clear` deletes them)\nrg output cap: {} bytes (`greeg stats enable --cap N` or `stats_cap` in the config file)",
        p.display(),
        d.display(),
        effective_cap(None)
    );
    if let Ok(v) = std::env::var("GREEG_STATS")
        && !env_on(&v)
    {
        println!("note: GREEG_STATS={v} is set in this shell and overrides the config file");
    }
    Ok(())
}

pub fn disable() -> Result<()> {
    let p = write_config_key("stats", "false")?;
    println!(
        "greeg stats: disabled in {} (existing records kept; `greeg stats clear` deletes them)",
        p.display()
    );
    if let Ok(v) = std::env::var("GREEG_STATS")
        && env_on(&v)
    {
        println!("note: GREEG_STATS={v} is set in this shell and overrides the config file");
    }
    Ok(())
}

pub fn status() -> Result<()> {
    let (on, src) = decide();
    let d = dir().context("no cache directory")?;
    let cfg = config_path();
    println!(
        "collection: {} (source: {})",
        if on { "on" } else { "off" },
        match src {
            Source::Env => "GREEG_STATS".to_string(),
            Source::Config => cfg
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            Source::Default => "default".to_string(),
        }
    );
    println!("records: {}", d.display());
    let ev = load_events(&d);
    let hooks = ev.iter().filter(|e| matches!(e, Event::Hook(_))).count();
    let runs = ev.len() - hooks;
    let replays = load_replays(&d).len();
    let (first, last) = ev
        .iter()
        .map(|e| match e {
            Event::Hook(h) => h.ts,
            Event::Run(r) => r.ts,
        })
        .fold((u64::MAX, 0), |(a, b), t| (a.min(t), b.max(t)));
    if ev.is_empty() {
        println!("events: none");
    } else {
        println!(
            "events: {hooks} hook rewrites, {runs} runs, {replays} replayed ({} → {})",
            date(first),
            date(last)
        );
    }
    println!("rg output cap: {} bytes", effective_cap(None));
    Ok(())
}

pub fn clear() -> Result<()> {
    let d = dir().context("no cache directory")?;
    let mut n = 0;
    for f in [
        "events.jsonl",
        "events.1.jsonl",
        "replay.jsonl",
        "replay.1.jsonl",
    ] {
        match std::fs::remove_file(d.join(f)) {
            Ok(()) => n += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => anyhow::bail!("remove {}: {e}", d.join(f).display()),
        }
    }
    println!("greeg stats: removed {n} files under {}", d.display());
    Ok(())
}

// ---------------------------------------------------------------- replay

pub struct ReplayOpts {
    pub filter: Filter,
    pub runs: usize,
    pub limit: Option<usize>,
    pub force: bool,
    pub timeout: Duration,
}

#[derive(Debug)]
struct Timed {
    ms: f64,
    bytes: usize,
    tokens: usize,
    exit: i32,
}

fn drain<R: Read + Send + 'static>(r: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(mut r) = r {
            let _ = r.read_to_end(&mut v);
        }
        v
    })
}

/// Run `argv` in `cwd` once, capturing both streams (what the model reads).
/// Killed past `timeout`: a recorded search on a huge tree must not hang the replay.
fn run_once(argv: &[String], cwd: &Path, greeg: bool, timeout: Duration) -> Result<Timed> {
    use std::process::{Command, Stdio};
    let mut cmd = if greeg {
        let mut c = Command::new(std::env::current_exe()?);
        c.args(&argv[1..]).arg("--no-session");
        c
    } else {
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]);
        c
    };
    cmd.current_dir(cwd)
        .env("GREEG_STATS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let t = Instant::now();
    let mut child = cmd
        .spawn()
        .with_context(|| format!("start {}", shell_join(argv)))?;
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if t.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("{} did not finish within {:?}", shell_join(argv), timeout);
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let ms = t.elapsed().as_secs_f64() * 1e3;
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    Ok(Timed {
        ms,
        bytes: stdout.len() + stderr.len(),
        tokens: greeg_query::tokens::rendered(&stdout) + greeg_query::tokens::rendered(&stderr),
        exit: status.code().unwrap_or(-1),
    })
}

/// One warm-up, then `runs` timed runs; median time, output from the last run.
fn run_n(
    argv: &[String],
    cwd: &Path,
    greeg: bool,
    runs: usize,
    timeout: Duration,
) -> Result<Timed> {
    let _ = run_once(argv, cwd, greeg, timeout)?;
    let mut times = Vec::with_capacity(runs);
    let mut last = None;
    for _ in 0..runs.max(1) {
        let t = run_once(argv, cwd, greeg, timeout)?;
        times.push(t.ms);
        last = Some(t);
    }
    let mut t = last.context("no runs")?;
    times.sort_by(|a, b| a.total_cmp(b));
    t.ms = pct(&times, 0.5);
    Ok(t)
}

/// Live queries run against an index; make sure the replay does too, so a
/// repository whose index was never built (or was evicted) is not timed as a
/// tree scan.
fn ensure_index(cwd: &Path, timeout: Duration) -> Result<()> {
    let dir = greeg_index::index_dir_for(cwd)?;
    if greeg_index::read_manifest(&dir).is_some() {
        return Ok(());
    }
    eprintln!(
        "greeg stats replay: building the index for {}",
        cwd.display()
    );
    let argv: Vec<String> = ["greeg", "index", "--quiet", "--root", "."]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let t = run_once(&argv, cwd, true, timeout.max(Duration::from_secs(600)))?;
    if t.exit != 0 {
        anyhow::bail!("greeg index exited {}", t.exit);
    }
    Ok(())
}

pub fn replay(o: &ReplayOpts) -> Result<()> {
    let dir = dir().context("no cache directory")?;
    let l = load(&dir, &o.filter);
    // unique ids, newest hook record first, only those a run actually followed
    let mut seen = std::collections::HashSet::new();
    let mut todo: Vec<&HookEvent> = Vec::new();
    let mut order: Vec<usize> = l.pair.iter().flatten().copied().collect();
    order.sort_by_key(|&hi| std::cmp::Reverse(l.hooks[hi].ts));
    for hi in order {
        let h = &l.hooks[hi];
        if !seen.insert(h.id.as_str()) {
            continue;
        }
        if !o.force && l.replays.contains_key(&h.id) {
            continue;
        }
        todo.push(h);
    }
    if let Some(n) = o.limit {
        todo.truncate(n);
    }
    if todo.is_empty() {
        eprintln!(
            "greeg stats replay: nothing to replay (every rewritten query already has a counterfactual; --force re-runs them)"
        );
        return Ok(());
    }
    eprintln!(
        "greeg stats replay: {} unique queries, {} timed runs each after a warm-up; run this on a quiet machine",
        todo.len(),
        o.runs.max(1)
    );
    let (mut done, mut skipped) = (0usize, 0usize);
    let mut indexed: std::collections::HashSet<PathBuf> = Default::default();
    for (i, h) in todo.iter().enumerate() {
        let cwd = Path::new(&h.cwd);
        if !cwd.is_dir() {
            eprintln!("greeg stats replay: skip {}: directory is gone", h.cwd);
            skipped += 1;
            continue;
        }
        if indexed.insert(cwd.to_path_buf())
            && let Err(e) = ensure_index(cwd, o.timeout)
        {
            eprintln!(
                "greeg stats replay: {}: {e:#}; timing against a scan",
                h.cwd
            );
        }
        let rg = match run_n(&h.original, cwd, false, o.runs, o.timeout) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("greeg stats replay: skip: {e:#}");
                skipped += 1;
                continue;
            }
        };
        let gg = match run_n(&h.rewritten, cwd, true, o.runs, o.timeout) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("greeg stats replay: skip: {e:#}");
                skipped += 1;
                continue;
            }
        };
        let ev = ReplayEvent {
            ts: greeg_index::now_ms(),
            id: h.id.clone(),
            runs: o.runs.max(1),
            rg_ms: rg.ms,
            rg_bytes: rg.bytes,
            rg_tokens: rg.tokens,
            rg_exit: rg.exit,
            greeg_ms: gg.ms,
            greeg_bytes: gg.bytes,
            greeg_tokens: gg.tokens,
            greeg_exit: gg.exit,
        };
        append_in(&dir, "replay.jsonl", &serde_json::to_string(&ev)?)?;
        done += 1;
        eprintln!(
            "{}/{} · rg {} ms {} tok · greeg {} ms {} tok",
            i + 1,
            todo.len(),
            ms(rg.ms),
            rg.tokens,
            ms(gg.ms),
            gg.tokens
        );
    }
    eprintln!(
        "greeg stats replay: {done} replayed, {skipped} skipped; `greeg stats` shows the comparison"
    );
    if done == 0 && skipped > 0 {
        anyhow::bail!("nothing could be replayed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "greeg-stats-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn config_subset() {
        let c =
            parse_config("# x\nstats = true\nstats_cap = 12000 # bytes\n[other]\nstats = false\n");
        assert_eq!(
            c,
            Config {
                enabled: Some(true),
                cap: Some(12000)
            }
        );
        assert_eq!(parse_config("stats = maybe\n"), Config::default());
        assert_eq!(parse_config(""), Config::default());
    }

    #[test]
    fn set_key_replaces_inserts_and_keeps_tables() {
        assert_eq!(set_key("", "stats", "true"), "stats = true\n");
        assert_eq!(
            set_key("# c\nstats = false\nother = 1\n", "stats", "true"),
            "# c\nstats = true\nother = 1\n"
        );
        // new key goes before the first table so it stays top-level
        assert_eq!(
            set_key("other = 1\n[lang]\nstats = 9\n", "stats", "true"),
            "other = 1\nstats = true\n[lang]\nstats = 9\n"
        );
        // a commented-out key is not the key
        assert_eq!(
            set_key("# stats = true\n", "stats", "false"),
            "# stats = true\nstats = false\n"
        );
        assert_eq!(parse_config(&set_key("", "stats_cap", "5")).cap, Some(5));
    }

    #[test]
    fn env_values() {
        for v in ["1", "true", "yes", "on", "anything"] {
            assert!(env_on(v), "{v}");
        }
        for v in ["", "0", "false", "off", "no", " FALSE "] {
            assert!(!env_on(v), "{v}");
        }
    }

    #[test]
    fn percentiles_nearest_rank() {
        let d = Dist::of((1..=100).map(|i| i as f64).collect());
        assert_eq!(
            (d.n, d.min, d.p50, d.p95, d.p99, d.max),
            (100, 1.0, 50.0, 95.0, 99.0, 100.0)
        );
        assert_eq!(d.avg, 50.5);
        let one = Dist::of(vec![7.0]);
        assert_eq!((one.p50, one.p99, one.total), (7.0, 7.0, 7.0));
        assert_eq!(Dist::of(vec![]).n, 0);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_since("7d").unwrap(), 7 * 86_400_000);
        assert_eq!(parse_since("90s").unwrap(), 90_000);
        assert_eq!(parse_since("1.5h").unwrap(), 5_400_000);
        assert_eq!(parse_since("2w").unwrap(), 14 * 86_400_000);
        assert!(parse_since("3y").is_err());
        assert!(parse_since("").is_err());
        assert!(parse_since("d").is_err());
    }

    #[test]
    fn pair_id_normalises_argv0_and_cwd() {
        let a = pair_id(
            Path::new("."),
            &["/usr/local/bin/greeg".into(), "foo".into()],
        );
        let b = pair_id(Path::new("."), &["greeg".into(), "foo".into()]);
        assert_eq!(a, b);
        assert_ne!(a, pair_id(Path::new("."), &["greeg".into(), "bar".into()]));
        assert_ne!(a, pair_id(Path::new(".."), &["greeg".into(), "foo".into()]));
        assert_eq!(a.len(), 16);
    }

    fn hook(ts: u64, id: &str) -> HookEvent {
        HookEvent {
            ts,
            id: id.into(),
            cwd: ".".into(),
            session: None,
            original: vec!["rg".into(), "-n".into(), "fn main".into()],
            rewritten: vec!["greeg".into(), "fn main".into()],
        }
    }
    fn run(ts: u64, id: &str) -> RunEvent {
        RunEvent {
            ts,
            id: id.into(),
            cwd: ".".into(),
            session: None,
            argv: vec!["greeg".into(), "fn main".into()],
            verb: "search".into(),
            wall_ms: 10.0,
            scan_ms: None,
            shape_ms: None,
            source: None,
            bytes: 100,
            tokens: 30,
            hits: None,
            files: None,
            exit: 0,
        }
    }
    fn replay_ev(id: &str, rg_ms: f64, rg_bytes: usize, rg_tokens: usize) -> ReplayEvent {
        ReplayEvent {
            ts: 0,
            id: id.into(),
            runs: 1,
            rg_ms,
            rg_bytes,
            rg_tokens,
            rg_exit: 0,
            greeg_ms: 5.0,
            greeg_bytes: 100,
            greeg_tokens: 30,
            greeg_exit: 0,
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn replay_full(
        id: &str,
        rg_ms: f64,
        rg_bytes: usize,
        rg_tokens: usize,
        rg_exit: i32,
        greeg_ms: f64,
        greeg_tokens: usize,
        greeg_exit: i32,
    ) -> ReplayEvent {
        ReplayEvent {
            ts: 0,
            id: id.into(),
            runs: 1,
            rg_ms,
            rg_bytes,
            rg_tokens,
            rg_exit,
            greeg_ms,
            greeg_bytes: greeg_tokens * 3,
            greeg_tokens,
            greeg_exit,
        }
    }
    fn put(d: &Path, name: &str, v: &impl Serialize) {
        append_in(d, name, &serde_json::to_string(v).unwrap()).unwrap();
    }

    #[test]
    fn pairing_is_nearest_unused_within_window() {
        let hooks = vec![hook(1000, "a"), hook(2000, "a"), hook(3000, "b")];
        let runs = vec![
            run(2100, "a"),
            run(1100, "a"),
            run(3050, "b"),
            run(3060, "b"),
            run(1_000_000, "a"),
            run(900, "a"), // before any hook
        ];
        let p = pair_runs(&hooks, &runs);
        assert_eq!(p, vec![Some(1), Some(0), Some(2), None, None, None]);
        assert!(pair_runs(&[], &runs).iter().all(Option::is_none));
    }

    #[test]
    fn capped_tokens_scale_with_bytes() {
        let r = replay_ev("x", 0.0, 90_000, 30_000);
        assert_eq!(capped_tokens(&r, 30_000), 10_000.0);
        assert_eq!(capped_tokens(&r, 0), 30_000.0);
        assert_eq!(capped_tokens(&r, 100_000), 30_000.0);
    }

    #[test]
    fn capture_head_extrapolates() {
        let head = b"fn main() { println!(\"hi\"); }\n".repeat(100);
        let t = greeg_query::tokens::rendered(&head);
        assert_eq!(estimate_tokens(&head, head.len()), t);
        assert_eq!(estimate_tokens(&head, head.len() * 3), t * 3);
        assert_eq!(estimate_tokens(&[], 5000), 0);
    }

    #[test]
    fn dates_and_thousands() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_756_857_600_000), "2025-09-03");
        assert_eq!(thousands(1234567.0), "1,234,567");
        assert_eq!(thousands(-1500.0), "-1,500");
        assert_eq!(thousands(999.0), "999");
        assert_eq!(ms(3.16), "3.2");
        assert_eq!(ms(-3.16), "-3.2");
        assert_eq!(ms(123.4), "123");
    }

    #[test]
    fn append_round_trips_skips_torn_lines_and_rotates() {
        let d = tmp();
        put(&d, "events.jsonl", &Event::Hook(hook(1, "a")));
        // a torn write from a crashed process: skipped, not fatal
        append_in(&d, "events.jsonl", "{\"kind\":\"run\",\"ts\":").unwrap();
        put(&d, "events.jsonl", &Event::Run(run(2, "a")));
        let ev = load_events(&d);
        assert_eq!(ev.len(), 2);
        assert!(matches!(&ev[0], Event::Hook(h) if h.original[2] == "fn main"));
        assert!(matches!(&ev[1], Event::Run(r) if r.tokens == 30));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.join("events.jsonl"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // rotation: an oversized file moves aside and both generations are read
        let f = std::fs::OpenOptions::new()
            .append(true)
            .open(d.join("events.jsonl"))
            .unwrap();
        f.set_len(ROTATE_BYTES + 1).unwrap();
        put(&d, "events.jsonl", &Event::Run(run(3, "a")));
        assert!(d.join("events.1.jsonl").exists());
        assert!(std::fs::metadata(d.join("events.jsonl")).unwrap().len() < 1000);
        assert_eq!(load_events(&d).len(), 3);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn report_classifies_and_computes_savings() {
        let d = tmp();
        let now = greeg_index::now_ms();
        // two rewritten runs of the same query, one direct search, one verb miss
        put(&d, "events.jsonl", &Event::Hook(hook(now - 5000, "a")));
        put(&d, "events.jsonl", &Event::Run(run(now - 4900, "a")));
        put(&d, "events.jsonl", &Event::Hook(hook(now - 3000, "a")));
        put(&d, "events.jsonl", &Event::Run(run(now - 2900, "a")));
        let mut direct = run(now - 2000, "zzz");
        direct.wall_ms = 30.0;
        put(&d, "events.jsonl", &Event::Run(direct));
        let mut verb = run(now - 1000, "v");
        verb.verb = "def".into();
        verb.exit = 1;
        put(&d, "events.jsonl", &Event::Run(verb));
        // rg took 25 ms and printed 60 000 bytes / 20 000 tokens; capped at 30 000 that is 10 000
        put(&d, "replay.jsonl", &replay_ev("a", 25.0, 60_000, 20_000));
        let opts = |cap, verbose, json| ReportOpts {
            filter: Filter::default(),
            cap,
            json,
            verbose,
        };
        let text = |o: &ReportOpts| {
            let mut out = Vec::new();
            report_to(&mut out, &d, o, (true, Source::Env), &[]).unwrap();
            String::from_utf8(out).unwrap()
        };
        let s = text(&opts(Some(30_000), true, false));
        assert!(
            s.contains("2 queries rewritten from rg/grep by the hook (2 replayed) · 1 direct search · 1 symbol verb (no rg counterfactual)"),
            "{s}"
        );
        // the headline: totals and typical (median) query, rg vs greeg vs saved
        assert!(s.contains("savings vs rg/grep · 2 replayed queries · same machine and tree · rg output capped at 30,000 bytes"), "{s}");
        assert!(
            s.contains("  tokens, total                20,000           60       19,940  100%"),
            "{s}"
        );
        assert!(s.contains("  tokens, typical query        10,000           30        9,970  greeg smaller on 2 queries, larger on 0"), "{s}");
        assert!(
            s.contains("  time, total                   50 ms        10 ms        40 ms  80%"),
            "{s}"
        );
        assert!(s.contains("  time, typical query           25 ms       5.0 ms        20 ms  greeg faster on 2 queries, slower on 0"), "{s}");
        // one query only: no concentration line, no misses
        assert!(!s.contains("largest win"), "{s}");
        assert!(!s.contains("found nothing"), "{s}");
        assert!(!s.contains("no rg counterfactual yet"), "{s}");
        assert!(!s.contains("not followed by a greeg run"), "{s}");
        // distributions, with the per-query saving as a row of its own
        assert!(s.contains("greeg, rewritten             2      10"), "{s}");
        assert!(s.contains("greeg, direct search         1      30"), "{s}");
        assert!(s.contains("greeg, verbs                 1      10"), "{s}");
        assert!(s.contains("rg/grep (replayed)           2      25"), "{s}");
        assert!(s.contains("saved per query              2      20      20      20      20      20      20          40"), "{s}");
        assert!(s.contains("rg/grep raw (replayed)       2  20,000"), "{s}");
        assert!(s.contains("rg/grep capped 30,000        2  10,000"), "{s}");
        assert!(s.contains("saved per query              2   9,970   9,970   9,970   9,970   9,970   9,970      19,940"), "{s}");
        // verbose: every replayed query, largest saving first
        assert!(
            s.contains("   n  saved tok  saved ms   rg tok greeg tok    rg ms  greeg ms  command"),
            "{s}"
        );
        assert!(
            s.contains(
                "   2      9,970        20   10,000        30       25       5.0  rg -n 'fn main'"
            ),
            "{s}"
        );
        assert!(s.contains("runs by directory"), "{s}");
        assert!(
            s.contains("     4         2       1      1           120  ."),
            "{s}"
        );

        // without --verbose the commands never appear
        let plain = text(&opts(Some(30_000), false, false));
        assert!(!plain.contains("fn main"), "{plain}");
        assert!(!plain.contains("runs by directory"), "{plain}");

        // the cap is configurable and 0 means uncapped
        assert!(
            text(&opts(Some(0), false, false)).contains("rg/grep capped 0             2  20,000")
        );

        // json carries the same numbers
        let j: serde_json::Value =
            serde_json::from_str(&text(&opts(Some(30_000), false, true))).unwrap();
        assert_eq!(j["runs"]["rewritten"], 2);
        assert_eq!(j["tokens"]["rg_capped_replay"]["avg"], 10_000.0);
        assert_eq!(j["tokens"]["saved_per_query"]["total"], 19_940.0);
        assert_eq!(j["latency_ms"]["rg_replay"]["p99"], 25.0);
        assert_eq!(j["latency_ms"]["saved_per_query"]["p50"], 20.0);
        assert_eq!(j["replayed"], 2);
        assert_eq!(j["savings"]["tokens"]["saved"], 19_940.0);
        assert_eq!(j["savings"]["tokens"]["greeg_smaller"], 2);
        assert_eq!(j["savings"]["time_ms"]["saved"], 40.0);
        assert_eq!(j["savings"]["top"]["tokens"]["runs"], 2);
        assert_eq!(j["savings"]["top"]["time_ms"]["saved"], 40.0);
        assert_eq!(j["savings"]["missed"], 0);
        assert_eq!(j["hooks_without_run"], 0);
        assert_eq!(j["queries"][0]["saved_tokens"], 9_970.0);
        assert!(j["queries"][0].get("command").is_none(), "{j}");
        let jv: serde_json::Value =
            serde_json::from_str(&text(&opts(Some(30_000), true, true))).unwrap();
        assert_eq!(jv["queries"][0]["command"], "rg -n 'fn main'");
        assert_eq!(jv["dirs"][0]["rewritten"], 2);

        // a filter that excludes everything says so instead of "no records"
        let f = ReportOpts {
            filter: Filter {
                since_ms: Some(1),
                repo: None,
                session: None,
            },
            cap: None,
            json: false,
            verbose: false,
        };
        assert!(text(&f).contains("no records match the filter (6 on disk)"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn report_shows_where_savings_come_from_and_what_went_wrong() {
        let d = tmp();
        let now = greeg_index::now_ms();
        for (i, id) in ["a", "b", "c", "d", "e"].iter().enumerate() {
            let t = now - 10_000 + i as u64 * 1000;
            put(&d, "events.jsonl", &Event::Hook(hook(t, id)));
            put(&d, "events.jsonl", &Event::Run(run(t + 100, id)));
        }
        // a hook record no greeg run followed (another rewriter won, or the call was cancelled)
        put(&d, "events.jsonl", &Event::Hook(hook(now - 500, "f")));
        // a: the one big win; b, d: greeg slightly larger and slower; c: rg found matches, greeg none; e: not replayed
        put(
            &d,
            "replay.jsonl",
            &replay_full("a", 574.0, 40_000, 12_000, 0, 3.0, 500, 0),
        );
        put(
            &d,
            "replay.jsonl",
            &replay_full("b", 3.0, 100, 40, 0, 6.0, 70, 0),
        );
        put(
            &d,
            "replay.jsonl",
            &replay_full("c", 4.0, 200, 60, 0, 7.0, 37, 1),
        );
        put(
            &d,
            "replay.jsonl",
            &replay_full("d", 2.0, 300, 80, 0, 5.0, 90, 0),
        );
        let text = |verbose: bool, json: bool| {
            let o = ReportOpts {
                filter: Filter::default(),
                cap: Some(30_000),
                json,
                verbose,
            };
            let mut out = Vec::new();
            report_to(
                &mut out,
                &d,
                &o,
                (true, Source::Config),
                &["rtk hook claude".into()],
            )
            .unwrap();
            String::from_utf8(out).unwrap()
        };
        let s = text(true, false);
        assert!(
            s.contains("5 queries rewritten from rg/grep by the hook (4 replayed)"),
            "{s}"
        );
        // capped rg tokens: a = 12 000 × 30 000 / 40 000 = 9 000
        assert!(
            s.contains("  tokens, total                 9,180          697        8,483  92%"),
            "{s}"
        );
        assert!(s.contains("  tokens, typical query            60           70          -10  greeg smaller on 2 queries, larger on 2"), "{s}");
        assert!(
            s.contains("  time, total                  583 ms        21 ms       562 ms  96%"),
            "{s}"
        );
        assert!(s.contains("  time, typical query          3.0 ms       5.0 ms      -3.0 ms  greeg faster on 1 query, slower on 3"), "{s}");
        assert!(
            s.contains("  the 2 largest token wins hold 8,523 tokens; the other 2 queries net -40 (--verbose lists every query)"),
            "{s}"
        );
        assert!(
            s.contains("  the largest time win holds 571 ms; the other 3 queries net -9.0 ms"),
            "{s}"
        );
        assert!(s.contains("  1 query found nothing where rg/grep had matches: a retry for the agent, not a saving (--verbose marks them with !)"), "{s}");
        assert!(
            s.contains("  1 rewritten query has no rg counterfactual yet: `greeg stats replay`"),
            "{s}"
        );
        assert!(
            s.contains("  1 hook rewrite was not followed by a greeg run: another PreToolUse hook rewrote the same call and finished last (candidates: `rtk hook claude`), or the call was cancelled"),
            "{s}"
        );
        assert!(s.contains("saved per query              4     140    -3.0    -3.0     571     571     571         562"), "{s}");
        // verbose: largest saving first, the miss marked
        let a = s
            .find(
                "   1      8,500       571    9,000       500      574       3.0  rg -n 'fn main'",
            )
            .expect(&s);
        let c = s
            .find(
                "   1!        23      -3.0       60        37      4.0       7.0  rg -n 'fn main'",
            )
            .expect(&s);
        let b = s
            .find(
                "   1        -30      -3.0       40        70      3.0       6.0  rg -n 'fn main'",
            )
            .expect(&s);
        assert!(a < c && c < b, "{s}");
        let j: serde_json::Value = serde_json::from_str(&text(false, true)).unwrap();
        assert_eq!(j["savings"]["tokens"]["saved"], 8483.0);
        assert_eq!(j["savings"]["tokens"]["median_per_query"], -10.0);
        assert_eq!(j["savings"]["time_ms"]["greeg_faster"], 1);
        assert_eq!(j["savings"]["top"]["tokens"]["queries"], 2);
        assert_eq!(j["savings"]["top"]["tokens"]["saved"], 8523.0);
        assert_eq!(j["savings"]["top"]["time_ms"]["queries"], 1);
        assert_eq!(j["savings"]["missed"], 1);
        assert_eq!(j["unreplayed"], 1);
        assert_eq!(j["hooks_without_run"], 1);
        assert_eq!(j["queries"][0]["id"], "a");
        assert_eq!(j["queries"][1]["missed"], true);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sessions_group_rewrites_direct_runs_and_lost_rewrites() {
        let d = tmp();
        let now = greeg_index::now_ms();
        let s1 = "aaaa1111-0000-0000-0000-000000000001";
        let s2 = "bbbb2222-0000-0000-0000-000000000002";
        // session 1: one rewritten query (replayed) and one rewrite lost to another hook
        let mut h = hook(now - 9000, "a");
        h.session = Some(s1.into());
        put(&d, "events.jsonl", &Event::Hook(h));
        put(&d, "events.jsonl", &Event::Run(run(now - 8900, "a")));
        let mut lost = hook(now - 8000, "b");
        lost.session = Some(s1.into());
        put(&d, "events.jsonl", &Event::Hook(lost));
        put(
            &d,
            "replay.jsonl",
            &replay_full("a", 25.0, 900, 300, 0, 5.0, 100, 0),
        );
        // session 2: a direct search and a verb, recorded with the env session id
        let mut direct = run(now - 5000, "zzz");
        direct.session = Some(s2.into());
        direct.cwd = "/repo/two".into();
        put(&d, "events.jsonl", &Event::Run(direct));
        let mut verb = run(now - 4000, "v");
        verb.session = Some(s2.into());
        verb.verb = "def".into();
        verb.cwd = "/repo/two".into();
        put(&d, "events.jsonl", &Event::Run(verb));
        // an older record with no session id
        put(&d, "events.jsonl", &Event::Run(run(now - 3000, "old")));
        let opts = |session: Option<&str>, json: bool| ReportOpts {
            filter: Filter {
                since_ms: None,
                repo: None,
                session: session.map(str::to_string),
            },
            cap: Some(30_000),
            json,
            verbose: false,
        };
        let mut out = Vec::new();
        sessions_to(&mut out, &d, &opts(None, false), Some(s2)).unwrap();
        let s = String::from_utf8(out).unwrap();
        // newest first, this session marked, one row per session
        let r2 = s
            .find("*bbbb2222          0        0            0     0.0 ms     0      1     1  /repo/two")
            .expect(&s);
        let r1 = s
            .find(" aaaa1111          1        1          200      20 ms     1      0     0  .")
            .expect(&s);
        assert!(r2 < r1, "{s}");
        assert!(s.contains("1 run has no session id"), "{s}");
        assert!(s.contains("* = this session"), "{s}");
        // the same numbers as json
        let mut out = Vec::new();
        sessions_to(&mut out, &d, &opts(None, true), None).unwrap();
        let j: serde_json::Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(j["sessions"][0]["session"], s2);
        assert_eq!(j["sessions"][1]["saved_tokens"], 200.0);
        assert_eq!(j["sessions"][1]["saved_ms"], 20.0);
        assert_eq!(j["sessions"][1]["lost"], 1);
        assert_eq!(j["runs_without_session"], 1);
        // the full report for one session, by prefix
        let mut out = Vec::new();
        report_to(
            &mut out,
            &d,
            &opts(Some("aaaa"), false),
            (true, Source::Env),
            &[],
        )
        .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("greeg stats · session aaaa · "), "{s}");
        assert!(
            s.contains("1 query rewritten from rg/grep by the hook (1 replayed) · 0 direct searches · 0 symbol verbs (no rg counterfactual)\n"),
            "{s}"
        );
        assert!(
            s.contains("  tokens, total                   300          100          200  67%"),
            "{s}"
        );
        assert!(
            s.contains("1 hook rewrite was not followed by a greeg run"),
            "{s}"
        );
        let mut out = Vec::new();
        report_to(&mut out, &d, &opts(None, true), (true, Source::Env), &[]).unwrap();
        let j: serde_json::Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(j["sessions"].as_array().unwrap().len(), 2);
        let mut out = Vec::new();
        report_to(
            &mut out,
            &d,
            &opts(Some("zzzz"), false),
            (true, Source::Env),
            &[],
        )
        .unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("no records for session zzzz (6 on disk)")
        );
        // no session ids at all
        let e = tmp().join("none");
        put(&e, "events.jsonl", &Event::Run(run(now, "x")));
        let mut out = Vec::new();
        sessions_to(&mut out, &e, &opts(None, false), None).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("no session ids in the records (1 on disk)")
        );
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(e.parent().unwrap());
    }

    #[test]
    fn report_without_replay_points_at_replay() {
        let d = tmp();
        let now = greeg_index::now_ms();
        put(&d, "events.jsonl", &Event::Hook(hook(now - 50, "a")));
        put(&d, "events.jsonl", &Event::Run(run(now, "a")));
        let o = ReportOpts {
            filter: Filter::default(),
            cap: None,
            json: false,
            verbose: true,
        };
        let mut out = Vec::new();
        report_to(&mut out, &d, &o, (true, Source::Config), &[]).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains("1 rewritten query has no rg counterfactual yet"),
            "{s}"
        );
        assert!(!s.contains("savings"), "{s}");
        assert!(!s.contains("rg/grep (replayed)"), "{s}");
        // empty dir: off vs on messages
        let e = tmp().join("empty");
        let mut out = Vec::new();
        report_to(&mut out, &e, &o, (false, Source::Default), &[]).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("collection is off")
        );
        let mut out = Vec::new();
        report_to(&mut out, &e, &o, (true, Source::Env), &[]).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("no records yet"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn human_units() {
        assert_eq!(human_ms(4394.0), "4.39 s");
        assert_eq!(human_ms(-1200.0), "-1.20 s");
        assert_eq!(human_ms(42.4), "42 ms");
        assert_eq!(human_ms(-2.29), "-2.3 ms");
        assert_eq!(datetime(1_756_857_600_000 + 39_720_000), "2025-09-03 11:02");
        assert_eq!(plural(1, "query", "queries"), "1 query");
        assert_eq!(plural(1200, "query", "queries"), "1,200 queries");
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(
            program_name("/opt/bin/git-ai checkpoint claude"),
            "git-ai checkpoint claude"
        );
        assert_eq!(program_name("rtk hook claude"), "rtk hook claude");
        assert_eq!(program_name("/x/y"), "y");
    }

    #[test]
    fn replay_run_once_times_kills_and_reports_missing() {
        let d = tmp();
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let t = run_once(
            &argv(&["echo", "hello world"]),
            &d,
            false,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!((t.bytes, t.exit), (12, 0));
        assert!(t.tokens >= 2 && t.ms < 5000.0);
        let t0 = Instant::now();
        let e = run_once(
            &argv(&["sleep", "5"]),
            &d,
            false,
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(e.to_string().contains("did not finish"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(4));
        assert!(
            run_once(
                &argv(&["greeg-no-such-binary-x"]),
                &d,
                false,
                Duration::from_secs(1)
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
