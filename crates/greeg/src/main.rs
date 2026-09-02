//! greeg CLI: ripgrep-compatible surface, symbol verbs, shaped output for agents.

mod doctor;
mod hook;
mod verbs_out;

use anyhow::Result;
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand};
use greeg_query::shape::{Layout, Report, ShownFile};
use greeg_query::{HitKind, Mode, Options, ScanResult};
use std::io::{BufWriter, Write};
use std::path::PathBuf;

const EXAMPLES: &str = "\
Examples:
  greeg get_queryset                ranked hits, definitions first, ~2k tokens
  greeg -w respond -t kt --mode files
  greeg \"fn poll_read\" --budget 400   phrase; budget in tokens (0 = unlimited, rg-ordered)
  greeg def JoinHandle              where is it defined (signature, doc, reachability)
  greeg refs Semaphore              references grouped by kind (call, type, import, …)
  greeg callers spawn_blocking --depth 2
  greeg outline src/lib.rs          symbols of one file as a tree
  greeg map src/                    important files by import PageRank
  greeg impact get_queryset         what breaks if it changes
  greeg -e def --kind def           a pattern that looks like a verb

Reading the output: `kind path:line  chain › text`. Kinds: def import call type
member ident doc comment string. Tests/vendored/generated files are demoted, never
hidden; the footer says what was cut and suggests the next query. Exit 1 = no hits.
The index builds itself in the background on first use; `greeg doctor` shows it.";

#[derive(Parser, Debug)]
#[command(name = "greeg", version, about = "A grep for coding agents: syntax-aware, ranked, budgeted. Accepts ripgrep flags.", after_help = EXAMPLES, disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Regex pattern (ripgrep syntax). Use -F for a literal, -e for a pattern that looks like a verb.
    pattern: Option<String>,
    /// Paths to search (default: current directory)
    paths: Vec<PathBuf>,
    /// Pattern (use when the pattern starts with `-` or equals a verb name)
    #[arg(short = 'e', long = "regexp", value_name = "PATTERN")]
    regexp: Option<String>,

    #[command(flatten)]
    common: Common,
}

/// Flags shared by search and the verbs (accepted before or after a verb).
#[derive(Args, Debug, Clone)]
struct Common {
    // ---- ripgrep-compatible ----
    /// Case-insensitive search
    #[arg(short = 'i', long = "ignore-case", global = true)]
    ignore_case: bool,
    /// Smart case: insensitive unless the pattern has uppercase
    #[arg(short = 'S', long = "smart-case", global = true)]
    smart_case: bool,
    /// Case-sensitive search (default)
    #[arg(short = 's', long = "case-sensitive", global = true)]
    case_sensitive: bool,
    /// Only match whole words
    #[arg(short = 'w', long = "word-regexp", global = true)]
    word: bool,
    /// Only match whole lines
    #[arg(short = 'x', long = "line-regexp", global = true)]
    line_regexp: bool,
    /// Treat the pattern as a literal string
    #[arg(short = 'F', long = "fixed-strings", global = true)]
    fixed_strings: bool,
    /// Multiline matching
    #[arg(short = 'U', long = "multiline", global = true)]
    multiline: bool,
    /// Show line numbers (always on; accepted for compatibility)
    #[arg(short = 'n', long = "line-number", action = ArgAction::SetTrue, global = true)]
    _line_number: bool,
    /// Only print paths with matches
    #[arg(short = 'l', long = "files-with-matches", global = true)]
    files_with_matches: bool,
    /// Only print the count of matching lines per file
    #[arg(short = 'c', long = "count", global = true)]
    count: bool,
    /// Lines of context after each hit
    #[arg(short = 'A', long = "after-context", value_name = "N", global = true)]
    after: Option<usize>,
    /// Lines of context before each hit
    #[arg(short = 'B', long = "before-context", value_name = "N", global = true)]
    before: Option<usize>,
    /// Lines of context around each hit
    #[arg(short = 'C', long = "context", value_name = "N", global = true)]
    context: Option<usize>,
    /// Include or exclude files by glob (prefix ! to exclude)
    #[arg(short = 'g', long = "glob", value_name = "GLOB", global = true)]
    globs: Vec<String>,
    /// Only search files of a language: py rs js ts kt (or full names)
    #[arg(short = 't', long = "type", value_name = "TYPE", global = true)]
    types: Vec<String>,
    /// Exclude files of a language
    #[arg(short = 'T', long = "type-not", value_name = "TYPE", global = true)]
    types_not: Vec<String>,
    /// Do not respect .gitignore / .ignore
    #[arg(long = "no-ignore", global = true)]
    no_ignore: bool,
    /// Search hidden files and directories
    #[arg(long = "hidden", global = true)]
    hidden: bool,
    /// Reader threads (default: 4 on macOS, all cores elsewhere)
    #[arg(short = 'j', long = "threads", default_value_t = 0, global = true)]
    threads: usize,
    /// Clip lines longer than N columns around the match (0 = never)
    #[arg(long = "max-columns", default_value_t = 200, global = true)]
    max_columns: usize,
    /// Skip files larger than this many bytes
    #[arg(long = "max-filesize", default_value_t = 4 << 20, global = true)]
    max_filesize: u64,
    /// JSON Lines output (ripgrep schema plus kind/symbol/facets/footer)
    #[arg(long = "json", global = true)]
    json: bool,

    // ---- greeg ----
    /// Output token budget (0 = unlimited, path order, no ranking)
    #[arg(long = "budget", default_value_t = 2000, global = true)]
    budget: usize,
    /// Output mode: files | outline | content | block
    #[arg(long = "mode", default_value = "content", global = true)]
    mode: String,
    /// Bias ranking toward these paths (repeatable)
    #[arg(long = "near", value_name = "PATH", global = true)]
    near: Vec<String>,
    /// Exclude test files entirely
    #[arg(long = "no-tests", global = true)]
    no_tests: bool,
    /// Exclude vendored files entirely
    #[arg(long = "no-vendored", global = true)]
    no_vendored: bool,
    /// Exclude generated, minified and lock files entirely
    #[arg(long = "no-generated", global = true)]
    no_generated: bool,
    /// Disable demotion of tests/vendored/generated files
    #[arg(long = "all", global = true)]
    all: bool,
    /// Only hits of these kinds: def import call type member ident doc comment string (comma-separated)
    #[arg(long = "kind", value_name = "KINDS", global = true)]
    kind: Option<String>,
    /// Disable the zero-hit escalation ladder
    #[arg(long = "no-ladder", global = true)]
    no_ladder: bool,
    /// Maximum hits shown per file
    #[arg(long = "per-file", default_value_t = 4, global = true)]
    per_file: usize,
    /// Root directory paths are reported relative to
    #[arg(long = "root", global = true)]
    root: Option<PathBuf>,
    /// Print scan statistics to stderr
    #[arg(long = "stats", global = true)]
    stats: bool,
    /// Freshness check for the index: auto | none | stat | fsevents
    #[arg(long = "fresh", default_value = "auto", global = true)]
    fresh: String,
    /// Do not use (or build) the index; scan the tree
    #[arg(long = "no-index", global = true)]
    no_index: bool,
    /// Index directory (default: per-repo directory under the user cache dir)
    #[arg(long = "index-dir", global = true)]
    index_dir: Option<PathBuf>,
    /// Re-parse shown files with tree-sitter for exact hit kinds (call/type/member)
    #[arg(long = "precise", global = true)]
    precise: bool,
    /// Session id for memory (default: the calling agent process; env GREEG_SESSION)
    #[arg(long = "session", value_name = "ID", global = true)]
    session: Option<String>,
    /// Disable session memory (dedup, focus, loop detection)
    #[arg(long = "no-session", global = true)]
    no_session: bool,
    /// Internal: re-executed after a SIGBUS on an index file
    #[arg(long = "after-sigbus", hide = true, global = true)]
    after_sigbus: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Build or inspect the index for a repository
    Index {
        /// Repository root (default: current directory)
        #[arg(long = "root", default_value = ".")]
        root: PathBuf,
        /// Index directory override
        #[arg(long = "index-dir")]
        index_dir: Option<PathBuf>,
        /// Print the manifest and exit
        #[arg(long = "status")]
        status: bool,
        /// Run a freshness check, apply it, and report what changed
        #[arg(long = "check")]
        check: bool,
        /// Freshness mode for --check: auto | stat | fsevents
        #[arg(long = "fresh", default_value = "stat")]
        fresh: String,
        /// Reader threads for the build (default: 4 on macOS, all cores elsewhere)
        #[arg(short = 'j', long = "threads", default_value_t = 0)]
        threads: usize,
        /// Grams only (no symbols, spans or graph)
        #[arg(long = "phase1")]
        phase1: bool,
        /// No progress output
        #[arg(long = "quiet")]
        quiet: bool,
    },
    /// Where is NAME defined? Ranked by kind, visibility, PageRank and reachability from --from
    Def {
        name: String,
        /// Origin file(s) for reachability ranking (default: files seen in this session)
        #[arg(long = "from", value_name = "FILE")]
        from: Vec<String>,
        /// Only this definition kind: fn method class struct enum trait interface type mod object const var macro field variant
        #[arg(long = "def-kind", value_name = "KIND")]
        def_kind: Option<String>,
    },
    /// References to NAME, grouped by kind (call, type, member, import, …)
    Refs { name: String },
    /// Functions that call NAME (grouped by enclosing symbol); --depth 2 adds who calls them
    Callers {
        name: String,
        #[arg(long = "depth", default_value_t = 1)]
        depth: usize,
    },
    /// Types that implement, extend or subclass NAME
    Impls { name: String },
    /// Definitions of one file as a tree
    Outline { file: String },
    /// The most important files and directories under DIR by import PageRank
    Map {
        #[arg(default_value = "")]
        dir: String,
    },
    /// What breaks if NAME changes: references split into will/may break/review, plus callers
    Impact { name: String },
    /// Index health, freshness mode, language coverage, disk use
    Doctor,
    /// Print the man page (roff) to stdout
    Man,
    /// Agent integrations: `greeg hook claude` installs the Claude Code hook and skill
    Hook {
        #[command(subcommand)]
        which: HookCmd,
    },
    /// Runtime-loaded languages: `greeg lang check DIR` validates and measures coverage
    Lang {
        #[command(subcommand)]
        which: LangCmd,
    },
}

#[derive(Subcommand, Debug)]
enum HookCmd {
    /// Install a PreToolUse hook that rewrites `rg`/`grep` Bash calls to `greeg`, and a skill file
    Claude {
        /// Remove the hook and the skill file
        #[arg(long)]
        uninstall: bool,
        /// Print what would change without writing
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// The hook itself: reads the tool call on stdin, prints a rewritten command (internal)
    Run,
}

#[derive(Subcommand, Debug)]
enum LangCmd {
    /// Validate every extra language and report capture coverage on files under DIR
    Check {
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
}

#[allow(clippy::too_many_arguments)]
fn run_index(root: PathBuf, index_dir: Option<PathBuf>, status: bool, check: bool, fresh: String, threads: usize, phase1: bool, quiet: bool) -> Result<()> {
    let dir = match index_dir {
        Some(d) => d,
        None => greeg_index::index_dir_for(&root)?,
    };
    if status {
        match greeg_index::read_manifest(&dir) {
            Some(m) => println!("{}", serde_json::to_string_pretty(&m)?),
            None => println!("no index at {}", dir.display()),
        }
        println!("dir: {}", dir.display());
        return Ok(());
    }
    if check {
        let idx = greeg_index::Index::open(&dir)?;
        let mode = greeg_index::fresh::Mode::parse(&fresh).ok_or_else(|| anyhow::anyhow!("bad --fresh"))?;
        let t = std::time::Instant::now();
        match greeg_index::fresh::check(&idx, &root, mode, 4) {
            Some(ch) => {
                println!("{}: modified {} deleted {} added {} added_dirs {} touched_dirs {} ignore_changed {} in {:.1} ms", ch.method, ch.modified.len(), ch.deleted.len(), ch.added.len(), ch.added_dirs.len(), ch.touched_dirs.len(), ch.ignore_changed, ch.ms);
                if greeg_index::fresh::needs_rebuild(&idx, &ch) {
                    println!("needs full rebuild");
                } else {
                    let n = greeg_index::fresh::apply(&idx, &root, &ch)?;
                    println!("applied delta with {n} files in {:.1} ms total", t.elapsed().as_secs_f64() * 1e3);
                }
            }
            None => println!("skipped (ttl or mode none)"),
        }
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    let marker = dir.join("BUILDING");
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&marker) {
        Ok(mut f) => {
            use std::io::Write as _;
            let _ = writeln!(f, "{}", std::process::id());
        }
        Err(_) => {
            // stale marker (older than 10 min) is taken over
            let stale = std::fs::metadata(&marker).ok().and_then(|m| m.modified().ok()).and_then(|m| m.elapsed().ok()).map(|e| e.as_secs() > 600).unwrap_or(true);
            if !stale {
                if !quiet {
                    eprintln!("greeg index: another build is running ({})", marker.display());
                }
                return Ok(());
            }
        }
    }
    let mut opts = greeg_index::build::BuildOpts { quiet, phase1_only: phase1, ..Default::default() };
    if threads > 0 {
        opts.reader_threads = threads;
    }
    let r = greeg_index::build::build(&root, &dir, &opts);
    let _ = std::fs::remove_file(&marker);
    let m = r?;
    if !quiet {
        eprintln!("greeg index: written to {} (generation {})", dir.display(), m.generation);
    }
    Ok(())
}

struct Argv(Vec<*const libc::c_char>);
unsafe impl Sync for Argv {}
unsafe impl Send for Argv {}
static REEXEC: std::sync::OnceLock<(Vec<std::ffi::CString>, Argv)> = std::sync::OnceLock::new();

/// SIGBUS means an mmapped index file was truncated underneath us. Re-exec the
/// same command with `--no-index --after-sigbus` (execv is async-signal-safe):
/// the answer comes from a scan and the index is rebuilt.
extern "C" fn on_sigbus(_: libc::c_int) {
    if let Some((_, argv)) = REEXEC.get() {
        unsafe {
            libc::execv(argv.0[0], argv.0.as_ptr());
        }
    }
    unsafe { libc::_exit(2) }
}

fn install_sigbus_guard() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.iter().any(|a| a == "--after-sigbus") || args.iter().any(|a| a == "--no-index") {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    use std::os::unix::ffi::OsStrExt;
    let mut cs: Vec<std::ffi::CString> = Vec::with_capacity(args.len() + 3);
    let Ok(e) = std::ffi::CString::new(exe.as_os_str().as_bytes()) else { return };
    cs.push(e);
    for a in args.iter().skip(1) {
        if let Ok(c) = std::ffi::CString::new(a.as_bytes()) {
            cs.push(c);
        }
    }
    cs.push(std::ffi::CString::new("--no-index").unwrap());
    cs.push(std::ffi::CString::new("--after-sigbus").unwrap());
    let mut ptrs: Vec<*const libc::c_char> = cs.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    let _ = REEXEC.set((cs, Argv(ptrs)));
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigbus as *const () as usize;
        sa.sa_flags = libc::SA_RESETHAND | libc::SA_NODEFER;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGBUS, &sa, std::ptr::null_mut());
    }
}

fn main() {
    let r = main_inner();
    greeg_query::indexed::flush_pending_build();
    if let Err(e) = r {
        eprintln!("greeg: {e:#}");
        std::process::exit(2);
    }
}

fn main_inner() -> Result<()> {
    // One line per panic, no backtrace hint: the index path is wrapped in
    // catch_unwind and degrades to a scan (DESIGN.md §12).
    std::panic::set_hook(Box::new(|info| {
        let msg = info.payload().downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| info.payload().downcast_ref::<String>().cloned()).unwrap_or_default();
        let loc = info.location().map(|l| format!(" at {}:{}", l.file(), l.line())).unwrap_or_default();
        eprintln!("greeg: internal error{loc}: {msg}");
    }));
    install_sigbus_guard();
    run()
}

fn build_options(c: &Common, pattern: String, paths: Vec<PathBuf>) -> Result<Options> {
    let root = c.root.clone().unwrap_or_else(|| PathBuf::from("."));
    let mode = if c.files_with_matches {
        Mode::Files
    } else if c.count {
        Mode::Count
    } else {
        match c.mode.as_str() {
            "files" => Mode::Files,
            "count" => Mode::Count,
            "outline" => Mode::Outline,
            "content" => Mode::Content,
            "block" => Mode::Block,
            m => anyhow::bail!("unknown --mode {m:?} (files|outline|content|block)"),
        }
    };
    let kinds = match &c.kind {
        Some(k) => k.split(',').map(|s| HitKind::parse(s.trim()).ok_or_else(|| anyhow::anyhow!("unknown --kind {s:?}"))).collect::<Result<Vec<_>>>()?,
        None => vec![],
    };
    Ok(Options {
        pattern,
        fixed_strings: c.fixed_strings,
        case_insensitive: c.ignore_case && !c.case_sensitive,
        smart_case: c.smart_case && !c.case_sensitive,
        word: c.word,
        line_regexp: c.line_regexp,
        multiline: c.multiline,
        root,
        paths,
        globs: c.globs.clone(),
        types: c.types.clone(),
        types_not: c.types_not.clone(),
        no_ignore: c.no_ignore,
        hidden: c.hidden,
        threads: c.threads,
        max_filesize: c.max_filesize,
        mode,
        budget: c.budget,
        near: c.near.clone(),
        no_tests: c.no_tests,
        no_vendored: c.no_vendored,
        no_generated: c.no_generated,
        all: c.all,
        kinds,
        ladder: !c.no_ladder,
        max_columns: c.max_columns,
        per_file_cap: c.per_file,
        context: c.context,
        before: c.before.unwrap_or(0),
        after: c.after.unwrap_or(0),
        use_index: !c.no_index,
        fresh: greeg_index::fresh::Mode::parse(&c.fresh).ok_or_else(|| anyhow::anyhow!("unknown --fresh {:?} (auto|none|stat|fsevents)", c.fresh))?,
        index_dir: c.index_dir.clone(),
        precise: c.precise,
    })
}

/// `GREEG_DEBUG_START=1`: print time since process start at main entry and after argument parsing.
fn since_start_us() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let r = unsafe { libc::proc_pidinfo(std::process::id() as libc::c_int, libc::PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut libc::c_void, size) };
        if r != size {
            return None;
        }
        let start = std::time::UNIX_EPOCH + std::time::Duration::new(info.pbi_start_tvsec, info.pbi_start_tvusec as u32 * 1000);
        return std::time::SystemTime::now().duration_since(start).ok().map(|d| d.as_micros() as u64);
    }
    #[allow(unreachable_code)]
    None
}

fn run() -> Result<()> {
    let trace = std::env::var_os("GREEG_DEBUG_START").is_some();
    if trace {
        eprintln!("greeg: main entry at {:?} µs after process start", since_start_us());
    }
    let cli = Cli::parse();
    if trace {
        eprintln!("greeg: args parsed at {:?} µs", since_start_us());
    }
    let c = &cli.common;
    if let Some(cmd) = cli.cmd {
        return match cmd {
            Cmd::Index { root, index_dir, status, check, fresh, threads, phase1, quiet } => run_index(root, index_dir, status, check, fresh, threads, phase1, quiet),
            Cmd::Def { name, from, def_kind } => verbs_out::run_def(c, &build_options(c, name.clone(), vec![])?, &name, &from, def_kind.as_deref()),
            Cmd::Refs { name } => verbs_out::run_refs(c, &build_options(c, name.clone(), vec![])?, &name),
            Cmd::Callers { name, depth } => verbs_out::run_callers(c, &build_options(c, name.clone(), vec![])?, &name, depth),
            Cmd::Impls { name } => verbs_out::run_impls(c, &build_options(c, name.clone(), vec![])?, &name),
            Cmd::Outline { file } => verbs_out::run_outline(c, &build_options(c, String::new(), vec![])?, &file),
            Cmd::Map { dir } => verbs_out::run_map(c, &build_options(c, String::new(), vec![])?, &dir),
            Cmd::Impact { name } => verbs_out::run_impact(c, &build_options(c, name.clone(), vec![])?, &name),
            Cmd::Doctor => doctor::run(c),
            Cmd::Man => {
                let mut out = Vec::new();
                clap_mangen::Man::new(Cli::command()).render(&mut out)?;
                std::io::stdout().write_all(&out)?;
                Ok(())
            }
            Cmd::Hook { which: HookCmd::Claude { uninstall, dry_run } } => hook::install_claude(uninstall, dry_run),
            Cmd::Hook { which: HookCmd::Run } => hook::run(),
            Cmd::Lang { which: LangCmd::Check { dir } } => doctor::lang_check(&dir),
        };
    }
    if c.after_sigbus {
        // the previous process died on a truncated index file: the index is unusable
        eprintln!("greeg: an index file was truncated while in use; answering from a scan and rebuilding the index");
        let o = build_options(c, String::new(), vec![])?;
        greeg_query::indexed::mark_corrupt(&o);
    }
    let (pattern, paths) = match (&cli.regexp, &cli.pattern) {
        (Some(e), Some(p)) => (e.clone(), std::iter::once(PathBuf::from(p)).chain(cli.paths.iter().cloned()).collect()),
        (Some(e), None) => (e.clone(), cli.paths.clone()),
        (None, Some(p)) => (p.clone(), cli.paths.clone()),
        (None, None) => anyhow::bail!("missing PATTERN (see --help)"),
    };
    let opts = build_options(c, pattern, paths)?;
    let session = if c.no_session { None } else { greeg_query::session::Session::open(&opts, c.session.as_deref()) };
    let mut opts = opts;
    if let Some(s) = &session
        && opts.near.is_empty()
    {
        opts.near = s.focus();
    }
    let t0 = std::time::Instant::now();
    let mut result = greeg_query::scan(&opts)?;
    let t_scan = t0.elapsed();
    let mut report = greeg_query::shape::shape(&mut result);
    if opts.precise {
        greeg_query::precise::apply(&mut result, &report);
    }
    if let Some(s) = &session {
        s.dedup(&result, &mut report);
        s.loop_hint(&opts, &result, &mut report);
    }
    let t_shape = t0.elapsed() - t_scan;
    let stdout = std::io::stdout();
    let mut w = BufWriter::with_capacity(64 * 1024, stdout.lock());
    if c.json {
        render_json(&mut w, &result, &report)?;
    } else {
        // render the body first so the footer can report a measured estimate
        let mut body: Vec<u8> = Vec::with_capacity(16 * 1024);
        render_body(&mut body, &result, &report)?;
        let est = greeg_query::tokens::rendered(body.len()) + 40;
        w.write_all(&body)?;
        render_footer(&mut w, &result, &report, est)?;
    }
    w.flush()?;
    if let Some(s) = &session {
        s.record(&opts, &result, &report);
    }
    if c.stats {
        let s = &result.stats;
        let t_render = t0.elapsed() - t_scan - t_shape;
        eprintln!("greeg: {} · walked {} files, candidates {}, searched {}, matched {} ({} hits) with {} threads; binary {} huge {}", s.source, s.files_walked, s.candidates, s.files_searched, s.files_matched, s.total_hits, s.threads, s.skipped_binary, s.skipped_huge);
        if s.source == "index" {
            eprintln!("greeg: fresh {} {:.1} ms ({} changed) · plan {}", s.fresh_method, s.fresh_ms, s.fresh_changed, if s.plan.len() > 120 { format!("{}…", &s.plan[..120]) } else { s.plan.clone() });
        }
        eprintln!("greeg: scan {:.1} ms · shape {:.1} ms (refine {:.1}) · render {:.1} ms · total {:.1} ms · cpu: read {:.1} search {:.1} classify {:.1} ms", t_scan.as_secs_f64() * 1e3, t_shape.as_secs_f64() * 1e3, s.refine_ms, t_render.as_secs_f64() * 1e3, t0.elapsed().as_secs_f64() * 1e3, s.cpu_read_ms, s.cpu_search_ms, s.cpu_classify_ms);
    }
    if result.stats.total_hits == 0 {
        greeg_query::indexed::flush_pending_build();
        std::process::exit(1);
    }
    Ok(())
}

pub(crate) fn fmt_n(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub(crate) fn fmt_size(b: u64) -> String {
    if b < 1024 {
        format!("{b} B")
    } else if b < 1024 * 1024 {
        format!("{} KB", b / 1024)
    } else {
        format!("{:.1} MB", b as f64 / 1048576.0)
    }
}

pub(crate) fn fmt_age(days: f32) -> String {
    if days < 1.0 {
        "today".into()
    } else if days < 30.0 {
        format!("{}d", days as u32)
    } else if days < 365.0 {
        format!("{}mo", (days / 30.0) as u32)
    } else {
        format!("{}y", (days / 365.0) as u32)
    }
}

pub(crate) fn chain_str(chain: &[(greeg_lang::DefKind, String)]) -> String {
    chain.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(" › ")
}

/// Chain to display for a hit: a definition's own name is already in its text.
pub(crate) fn hit_chain(h: &greeg_query::Hit) -> String {
    if h.kind == HitKind::Def && !h.chain.is_empty() { chain_str(&h.chain[..h.chain.len() - 1]) } else { chain_str(&h.chain) }
}

fn file_header(f: &greeg_query::FileResult) -> String {
    let mut flags = f.flags.names();
    flags.retain(|x| *x != "huge");
    let fl = if flags.is_empty() { String::new() } else { format!("  [{}]", flags.join(",")) };
    format!("{}{}  {} · {}", f.rel, fl, fmt_size(f.size), fmt_age(f.age_days))
}

fn render_body(w: &mut impl Write, r: &ScanResult, rep: &Report) -> Result<()> {
    let files = &r.files;
    match rep.layout {
        Layout::Files => {
            for sf in &rep.files {
                let f = &files[sf.file];
                let flags = f.flags.names();
                let fl = if flags.is_empty() { String::new() } else { format!("  [{}]", flags.join(",")) };
                writeln!(w, "{}  {} hits{}", f.rel, f.total, fl)?;
            }
        }
        Layout::Count => {
            for sf in &rep.files {
                writeln!(w, "{}:{}", files[sf.file].rel, files[sf.file].total)?;
            }
        }
        Layout::Outline => {
            for sf in &rep.files {
                let f = &files[sf.file];
                for sh in &sf.hits {
                    let h = &f.hits[sh.hit];
                    let text = String::from_utf8_lossy(&h.text);
                    let ch = hit_chain(h);
                    writeln!(w, "{:<7} {}:{}  {}{}{}", h.kind.name(), f.rel, h.line, ch, if ch.is_empty() { "" } else { " › " }, text.trim())?;
                }
                if sf.more > 0 {
                    writeln!(w, "        {}  +{} more", f.rel, sf.more)?;
                }
            }
        }
        Layout::Content | Layout::Block => {
            let mut first = true;
            for sf in &rep.files {
                let f = &files[sf.file];
                if !first {
                    writeln!(w)?;
                }
                first = false;
                writeln!(w, "{}", file_header(f))?;
                render_hits(w, f, sf)?;
            }
        }
        Layout::Facets => {
            let fc = rep.facets.as_ref().unwrap();
            writeln!(w, "{}  {} matches · {} files · exact-length {}", r.opts.pattern, fmt_n(r.stats.total_hits), fmt_n(r.stats.files_matched), fmt_n(fc.word_hits))?;
            let kinds: Vec<String> = fc.by_kind.iter().map(|(k, n)| format!("{} {}", k.name(), fmt_n(*n))).collect();
            writeln!(w, "by kind   {}", kinds.join("  "))?;
            let dirs: Vec<String> = fc.by_dir.iter().map(|(d, n)| format!("{}/ {}", d, fmt_n(*n))).collect();
            writeln!(w, "by area   {}", dirs.join("  "))?;
            let langs: Vec<String> = fc.by_lang.iter().map(|(l, n)| format!("{} {}", l, fmt_n(*n))).collect();
            writeln!(w, "by lang   {}", langs.join("  "))?;
            let flags: Vec<String> = fc.by_flag.iter().map(|(l, n)| format!("{} {}", l, fmt_n(*n))).collect();
            writeln!(w, "by flag   {}", flags.join("  "))?;
            if !fc.top_defs.is_empty() {
                writeln!(w, "\ndefinitions ({} of {})", fc.top_defs.len(), fc.defs_total)?;
                for &(fi, hi) in &fc.top_defs {
                    write_hit_line(w, &files[fi], &files[fi].hits[hi])?;
                }
            }
            if !fc.top_hits.is_empty() {
                writeln!(w, "\ntop hits")?;
                for &(fi, hi) in &fc.top_hits {
                    write_hit_line(w, &files[fi], &files[fi].hits[hi])?;
                }
            }
        }
    }
    Ok(())
}

fn render_footer(w: &mut impl Write, r: &ScanResult, rep: &Report, est: usize) -> Result<()> {
    let ft = &rep.footer;
    writeln!(w)?;
    if r.stats.total_hits == 0 {
        write!(w, "no hits")?;
    } else {
        write!(w, "{} of {} hits · {} of {} files", fmt_n(ft.hits_shown), fmt_n(ft.hits_total), fmt_n(ft.files_shown), fmt_n(ft.files_total))?;
        if ft.demoted_files > 0 {
            write!(w, " · demoted {} files ({} hits)", ft.demoted_files, fmt_n(ft.demoted_hits))?;
        }
    }
    if ft.skipped_binary + ft.skipped_huge > 0 {
        write!(w, " · skipped {} binary, {} huge", ft.skipped_binary, ft.skipped_huge)?;
    }
    if ft.rung != greeg_query::Rung::Exact {
        write!(w, " · matched {}", ft.rung.describe())?;
    }
    writeln!(w, " · ~{} tokens · {:.0} ms", est, ft.elapsed_ms)?;
    if !ft.hints.is_empty() {
        writeln!(w, "next: {}", ft.hints.join("  |  "))?;
    }
    Ok(())
}

fn write_hit_line(w: &mut impl Write, f: &greeg_query::FileResult, h: &greeg_query::Hit) -> Result<()> {
    let ch = hit_chain(h);
    let text = String::from_utf8_lossy(&h.text);
    let flag = if f.flags.demoted() { format!(" [{}]", f.flags.names().join(",")) } else { String::new() };
    writeln!(w, "  {:<7} {}:{}{}  {}{}{}", h.kind.name(), f.rel, h.line, flag, ch, if ch.is_empty() { "" } else { " › " }, text.trim())?;
    Ok(())
}

fn render_hits(w: &mut impl Write, f: &greeg_query::FileResult, sf: &ShownFile) -> Result<()> {
    let mut last_line_printed = 0u32;
    for sh in &sf.hits {
        let h = &f.hits[sh.hit];
        if let Some((first, lines)) = &sh.block {
            writeln!(w, "  ── {} {} (lines {}–{})", h.chain.last().map(|(k, _)| k.name()).unwrap_or("block"), chain_str(&h.chain), first, first + lines.len() as u32 - 1)?;
            for (i, l) in lines.iter().enumerate() {
                writeln!(w, "  {:>5}  {}", first + i as u32, String::from_utf8_lossy(l))?;
            }
            continue;
        }
        if let Some((first, lines)) = &sh.context {
            if last_line_printed > 0 && *first > last_line_printed + 1 {
                writeln!(w, "  ...")?;
            }
            for (i, l) in lines.iter().enumerate() {
                let ln = first + i as u32;
                if ln <= last_line_printed {
                    continue;
                }
                if ln == h.line {
                    let ch = hit_chain(h);
                    writeln!(w, "  {:>5} {:<7} {}{}", ln, h.kind.name(), String::from_utf8_lossy(&h.text), if ch.is_empty() { String::new() } else { format!("    ‹ {ch}") })?;
                } else {
                    writeln!(w, "  {:>5}         {}", ln, String::from_utf8_lossy(l))?;
                }
                last_line_printed = ln;
            }
        } else {
            let ch = hit_chain(h);
            let seen = if sh.seen_before { "    (shown before)" } else { "" };
            writeln!(w, "  {:>5} {:<7} {}{}{}", h.line, h.kind.name(), String::from_utf8_lossy(&h.text), if ch.is_empty() { String::new() } else { format!("    ‹ {ch}") }, seen)?;
            last_line_printed = h.line;
        }
    }
    if sf.more > 0 {
        let f_kinds: Vec<String> = HitKind::ALL.iter().filter(|k| f.kinds[k.idx()] > 0).map(|k| format!("{} {}", f.kinds[k.idx()], k.name())).collect();
        writeln!(w, "        +{} more in this file ({})", sf.more, f_kinds.join(", "))?;
    }
    Ok(())
}

fn render_json(w: &mut impl Write, r: &ScanResult, rep: &Report) -> Result<()> {
    use serde_json::json;
    let files = &r.files;
    let emit_file = |w: &mut dyn Write, fi: usize, hits: &[usize]| -> Result<()> {
        let f = &files[fi];
        serde_json::to_writer(&mut *w, &json!({"type":"begin","data":{"path":{"text":f.rel}}}))?;
        writeln!(w)?;
        for &hi in hits {
            let h = &f.hits[hi];
            let sym = h.chain.last().map(|(k, n)| json!({"name": n, "kind": k.name(), "container": chain_str(&h.chain[..h.chain.len()-1])}));
            serde_json::to_writer(&mut *w, &json!({"type":"match","data":{
                "path":{"text":f.rel},
                "lines":{"text":String::from_utf8_lossy(&h.text)},
                "line_number":h.line,
                "absolute_offset":h.line_start,
                "submatches":[{"match":{"text":String::from_utf8_lossy(&h.text[h.text_match.0 as usize..h.text_match.1 as usize])}, "start":h.text_match.0, "end":h.text_match.1}],
                "absolute_match_offset":h.match_start,
                "kind":h.kind.name(),
                "symbol":sym,
                "file_flags":f.flags.names(),
                "score":h.score,
                "clipped":h.clipped
            }}))?;
            writeln!(w)?;
        }
        serde_json::to_writer(&mut *w, &json!({"type":"end","data":{"path":{"text":f.rel},"stats":{"matches":f.total,"shown":hits.len()}}}))?;
        writeln!(w)?;
        Ok(())
    };
    match rep.layout {
        Layout::Files | Layout::Count => {
            for sf in &rep.files {
                emit_file(w, sf.file, &[])?;
            }
        }
        Layout::Facets => {
            let fc = rep.facets.as_ref().unwrap();
            serde_json::to_writer(&mut *w, &json!({"type":"facets","data":{
                "total":r.stats.total_hits,"files":r.stats.files_matched,"exact_length":fc.word_hits,
                "by_kind":fc.by_kind.iter().map(|(k,n)| json!([k.name(), n])).collect::<Vec<_>>(),
                "by_dir":fc.by_dir,"by_lang":fc.by_lang,"by_flag":fc.by_flag,
                "definitions_total":fc.defs_total
            }}))?;
            writeln!(w)?;
            for sf in &rep.files {
                let hits: Vec<usize> = sf.hits.iter().map(|h| h.hit).collect();
                emit_file(w, sf.file, &hits)?;
            }
        }
        _ => {
            for sf in &rep.files {
                let hits: Vec<usize> = sf.hits.iter().map(|h| h.hit).collect();
                emit_file(w, sf.file, &hits)?;
            }
        }
    }
    let ft = &rep.footer;
    serde_json::to_writer(&mut *w, &json!({"type":"footer","data":{
        "hits_shown":ft.hits_shown,"hits_total":ft.hits_total,"files_shown":ft.files_shown,"files_total":ft.files_total,
        "demoted_files":ft.demoted_files,"demoted_hits":ft.demoted_hits,"skipped_binary":ft.skipped_binary,"skipped_huge":ft.skipped_huge,
        "rung":ft.rung.name(),"rung_names":match &ft.rung { greeg_query::Rung::SplitTokens(v) | greeg_query::Rung::Fuzzy(v) => v.clone(), _ => vec![] },"ignored_only":ft.ignored_only,"est_tokens":ft.est_tokens,"elapsed_ms":ft.elapsed_ms,"hints":ft.hints,
        "layout":format!("{:?}", rep.layout).to_lowercase()
    }}))?;
    writeln!(w)?;
    Ok(())
}
