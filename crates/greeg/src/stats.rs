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
    let pair = pair_runs(&hooks, &runs);
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

fn row(w: &mut impl Write, label: &str, d: &Dist, f: fn(f64) -> String) -> Result<()> {
    if d.n == 0 {
        return Ok(());
    }
    writeln!(
        w,
        "{label:<24}{:>6}{:>8}{:>8}{:>8}{:>8}{:>8}{:>8}{:>12}",
        d.n,
        f(d.avg),
        f(d.min),
        f(d.p50),
        f(d.p95),
        f(d.p99),
        f(d.max),
        thousands(d.total)
    )?;
    Ok(())
}

fn head(w: &mut impl Write, unit: &str) -> Result<()> {
    writeln!(
        w,
        "{unit:<24}{:>6}{:>8}{:>8}{:>8}{:>8}{:>8}{:>8}{:>12}",
        "n", "avg", "min", "p50", "p95", "p99", "max", "total"
    )?;
    Ok(())
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
    report_to(&mut w, &dir, o, decide())
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|w| crate::hook::quote(w))
        .collect::<Vec<_>>()
        .join(" ")
}

fn report_to(w: &mut impl Write, dir: &Path, o: &ReportOpts, on: (bool, Source)) -> Result<()> {
    let l = load(dir, &o.filter);
    let cap = effective_cap(o.cap);
    if l.runs.is_empty() && l.hooks.is_empty() {
        if l.total > 0 {
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

    // latency and tokens, live
    let mut rewritten = Vec::new();
    let mut direct = Vec::new();
    let mut verbs = Vec::new();
    let mut tok_rewritten = Vec::new();
    let mut tok_direct = Vec::new();
    let mut tok_verbs = Vec::new();
    for (i, r) in l.runs.iter().enumerate() {
        if r.verb != "search" {
            verbs.push(r.wall_ms);
            tok_verbs.push(r.tokens as f64);
        } else if l.pair[i].is_some() {
            rewritten.push(r.wall_ms);
            tok_rewritten.push(r.tokens as f64);
        } else {
            direct.push(r.wall_ms);
            tok_direct.push(r.tokens as f64);
        }
    }
    let (n_rewritten, n_direct, n_verbs) = (rewritten.len(), direct.len(), verbs.len());
    let lat_rewritten = Dist::of(rewritten);
    let lat_direct = Dist::of(direct);
    let lat_verbs = Dist::of(verbs);
    let tk_rewritten = Dist::of(tok_rewritten);
    let tk_direct = Dist::of(tok_direct);
    let tk_verbs = Dist::of(tok_verbs);

    // replay: weight each replayed id by how many paired runs used it
    let mut rg_ms = Vec::new();
    let mut gg_ms = Vec::new();
    let mut rg_raw = Vec::new();
    let mut rg_cap = Vec::new();
    let mut gg_tok = Vec::new();
    let mut per_id: HashMap<&str, usize> = HashMap::new();
    let mut unreplayed = 0usize;
    for (i, r) in l.runs.iter().enumerate() {
        if l.pair[i].is_none() {
            continue;
        }
        let Some(rp) = l.replays.get(&r.id) else {
            unreplayed += 1;
            continue;
        };
        *per_id.entry(rp.id.as_str()).or_default() += 1;
        rg_ms.push(rp.rg_ms);
        gg_ms.push(rp.greeg_ms);
        rg_raw.push(rp.rg_tokens as f64);
        rg_cap.push(capped_tokens(rp, cap));
        gg_tok.push(rp.greeg_tokens as f64);
    }
    let n_replayed = rg_ms.len();
    let lat_rg = Dist::of(rg_ms);
    let lat_gg_replay = Dist::of(gg_ms);
    let tk_rg_raw = Dist::of(rg_raw);
    let tk_rg_cap = Dist::of(rg_cap);
    let tk_gg_replay = Dist::of(gg_tok);
    let (first, last) = span(&l);

    if o.json {
        let out = serde_json::json!({
            "from": date(first), "to": date(last), "cap": cap,
            "runs": {"rewritten": n_rewritten, "direct": n_direct, "verbs": n_verbs},
            "hooks": l.hooks.len(),
            "latency_ms": {"rewritten": lat_rewritten, "direct": lat_direct, "verbs": lat_verbs,
                            "rg_replay": lat_rg, "greeg_replay": lat_gg_replay},
            "tokens": {"rewritten": tk_rewritten, "direct": tk_direct, "verbs": tk_verbs,
                       "rg_raw_replay": tk_rg_raw, "rg_capped_replay": tk_rg_cap, "greeg_replay": tk_gg_replay},
            "replayed": n_replayed, "unreplayed": unreplayed,
        });
        writeln!(w, "{}", serde_json::to_string_pretty(&out)?)?;
        return Ok(());
    }

    writeln!(
        w,
        "greeg stats · {} → {} · {} runs ({} rewritten from rg/grep, {} direct searches, {} symbol verbs) · {} hook rewrites",
        date(first),
        date(last),
        l.runs.len(),
        n_rewritten,
        n_direct,
        n_verbs,
        l.hooks.len()
    )?;
    writeln!(w)?;
    head(w, "latency (ms)")?;
    row(w, "greeg, rewritten", &lat_rewritten, ms)?;
    row(w, "greeg, direct search", &lat_direct, ms)?;
    row(w, "greeg, verbs", &lat_verbs, ms)?;
    row(w, "greeg (replayed)", &lat_gg_replay, ms)?;
    row(w, "rg/grep (replayed)", &lat_rg, ms)?;
    writeln!(w)?;
    head(w, "tokens (est. o200k)")?;
    row(w, "greeg, rewritten", &tk_rewritten, thousands)?;
    row(w, "greeg, direct search", &tk_direct, thousands)?;
    row(w, "greeg, verbs", &tk_verbs, thousands)?;
    row(w, "greeg (replayed)", &tk_gg_replay, thousands)?;
    row(w, "rg/grep raw (replayed)", &tk_rg_raw, thousands)?;
    row(
        w,
        &format!("rg/grep capped {}", thousands(cap as f64)),
        &tk_rg_cap,
        thousands,
    )?;
    writeln!(w)?;
    if n_replayed > 0 {
        let dt = lat_rg.avg - lat_gg_replay.avg;
        let dtok = tk_rg_cap.avg - tk_gg_replay.avg;
        let pct_t = if lat_rg.avg > 0.0 {
            dt / lat_rg.avg * 100.0
        } else {
            0.0
        };
        let pct_k = if tk_rg_cap.avg > 0.0 {
            dtok / tk_rg_cap.avg * 100.0
        } else {
            0.0
        };
        writeln!(
            w,
            "savings vs rg/grep over {} replayed queries (same conditions, rg capped at {} bytes): {} ms/query ({:.0}%), {} tokens/query ({:.0}%), {} tokens in total",
            n_replayed,
            thousands(cap as f64),
            ms(dt),
            pct_t,
            thousands(dtok),
            pct_k,
            thousands(tk_rg_cap.total - tk_gg_replay.total)
        )?;
        if n_replayed < 100 {
            writeln!(
                w,
                "note: p95/p99 need a few hundred queries before they mean much"
            )?;
        }
    }
    if unreplayed > 0 {
        writeln!(
            w,
            "{unreplayed} rewritten queries have no rg counterfactual yet: `greeg stats replay` runs the original commands on a quiet machine"
        )?;
    }
    if o.verbose && !per_id.is_empty() {
        writeln!(w)?;
        writeln!(
            w,
            "{:>4} {:>8} {:>8} {:>8} {:>8}  command",
            "n", "greeg", "rg", "gg tok", "rg tok"
        )?;
        let mut ids: Vec<(&str, usize)> = per_id.into_iter().collect();
        ids.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        for (id, n) in ids {
            let rp = &l.replays[id];
            let cmd = l
                .hooks
                .iter()
                .find(|h| h.id == id)
                .map(|h| shell_join(&h.original))
                .unwrap_or_default();
            writeln!(
                w,
                "{n:>4} {:>8} {:>8} {:>8} {:>8}  {cmd}",
                ms(rp.greeg_ms),
                ms(rp.rg_ms),
                rp.greeg_tokens,
                capped_tokens(rp, cap).round() as usize
            )?;
        }
    }
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
            report_to(&mut out, &d, o, (true, Source::Env)).unwrap();
            String::from_utf8(out).unwrap()
        };
        let s = text(&opts(Some(30_000), true, false));
        assert!(
            s.contains("4 runs (2 rewritten from rg/grep, 1 direct searches, 1 symbol verbs) · 2 hook rewrites"),
            "{s}"
        );
        assert!(s.contains("greeg, rewritten             2      10"), "{s}");
        assert!(s.contains("greeg, direct search         1      30"), "{s}");
        assert!(s.contains("greeg, verbs                 1      10"), "{s}");
        assert!(s.contains("rg/grep (replayed)           2      25"), "{s}");
        assert!(s.contains("rg/grep raw (replayed)       2  20,000"), "{s}");
        assert!(s.contains("rg/grep capped 30,000        2  10,000"), "{s}");
        // 25 - 5 = 20 ms (80%), 10 000 - 30 = 9 970 tokens (100%), 2 × 9 970 in total
        assert!(
            s.contains("20 ms/query (80%), 9,970 tokens/query (100%), 19,940 tokens in total"),
            "{s}"
        );
        assert!(
            s.contains("   2      5.0       25       30    10000  rg -n 'fn main'"),
            "{s}"
        );
        assert!(!s.contains("no rg counterfactual"), "{s}");

        // without --verbose the commands never appear
        assert!(!text(&opts(Some(30_000), false, false)).contains("fn main"));

        // the cap is configurable and 0 means uncapped
        assert!(
            text(&opts(Some(0), false, false)).contains("rg/grep capped 0             2  20,000")
        );

        // json carries the same numbers
        let j: serde_json::Value =
            serde_json::from_str(&text(&opts(Some(30_000), false, true))).unwrap();
        assert_eq!(j["runs"]["rewritten"], 2);
        assert_eq!(j["tokens"]["rg_capped_replay"]["avg"], 10_000.0);
        assert_eq!(j["latency_ms"]["rg_replay"]["p99"], 25.0);
        assert_eq!(j["replayed"], 2);

        // a filter that excludes everything says so instead of "no records"
        let f = ReportOpts {
            filter: Filter {
                since_ms: Some(1),
                repo: None,
            },
            cap: None,
            json: false,
            verbose: false,
        };
        assert!(text(&f).contains("no records match the filter (6 on disk)"));
        let _ = std::fs::remove_dir_all(&d);
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
        report_to(&mut out, &d, &o, (true, Source::Config)).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains("1 rewritten queries have no rg counterfactual yet"),
            "{s}"
        );
        assert!(!s.contains("savings"), "{s}");
        assert!(!s.contains("rg/grep (replayed)"), "{s}");
        // empty dir: off vs on messages
        let e = tmp().join("empty");
        let mut out = Vec::new();
        report_to(&mut out, &e, &o, (false, Source::Default)).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("collection is off")
        );
        let mut out = Vec::new();
        report_to(&mut out, &e, &o, (true, Source::Env)).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("no records yet"));
        let _ = std::fs::remove_dir_all(&d);
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
