//! greeg CLI: ripgrep-compatible surface, symbol verbs, shaped output for agents.

mod doctor;
mod hook;
mod hook_config;
mod hook_skill;
mod stats;
mod verbs_out;

use anyhow::Result;
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand};
use greeg_query::outcome::Outcome;
use greeg_query::shape::{Layout, Report, ShownFile};
use greeg_query::{HitKind, MatchingPolicy, Mode, Options, ScanResult};
use std::io::{BufWriter, Write};
use std::path::PathBuf;

const EXAMPLES: &str = "\
Examples:
  greeg get_queryset                ranked hits grouped by file, definitions first, ~2k tokens
  greeg -w respond -t kt -l
  greeg \"fn poll_read\" --budget 400   phrase; budget in tokens (0 = unlimited, rg-shaped path:line:text)
  greeg def JoinHandle              where is it defined (signature, doc, reachability)
  greeg refs Semaphore              references grouped by kind (call, type, import, …)
  greeg callers spawn_blocking --depth 2
  greeg outline src/lib.rs          symbols of one file as a tree
  greeg show src/lib.rs:120         the definition enclosing a line, whole (def NAME --mode block: bodies)
  greeg map src/                    important files by import PageRank
  greeg impact get_queryset         what breaks if it changes
  greeg -e def --kind def           a pattern that looks like a verb

Reading the output: one header line per file (`path [test]`), then
`line kind text  ‹ container` beneath it; the container is the enclosing
`Class.method` (full chain with --chain). Kinds: def import call type member
ident (blank) doc comment string. Tests/vendored/generated files are demoted, never
hidden; the footer says what was cut and suggests flags for the next query.
Exit 1 = no hits for the pattern as given (a relaxed match is reported, exit 1).
The index builds itself in the background on first use; `greeg doctor` shows it.";

#[derive(Parser, Debug)]
#[command(name = "greeg", version = stats::VERSION, about = "A grep for coding agents: syntax-aware, ranked, budgeted. Accepts ripgrep flags.", after_help = EXAMPLES, disable_help_subcommand = true, disable_help_flag = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Regex pattern (ripgrep syntax). Use -F for a literal, -e for a pattern that looks like a verb.
    pattern: Option<String>,
    /// Paths to search (default: current directory; stdin when piped and no path is given)
    paths: Vec<PathBuf>,
    /// Pattern (repeatable: patterns are joined as alternatives; use when the pattern starts with `-` or equals a verb name)
    #[arg(
        short = 'e',
        long = "regexp",
        value_name = "PATTERN",
        allow_hyphen_values = true,
        num_args = 1
    )]
    regexp: Vec<String>,

    #[command(flatten)]
    common: Common,
}

/// Flags shared by search and the verbs (accepted before or after a verb).
#[derive(Args, Debug, Clone)]
struct Common {
    /// Print help
    #[arg(long = "help", action = ArgAction::Help, global = true)]
    _help: Option<bool>,

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
    /// Show line numbers (always on in ranked output; for stdin as in ripgrep)
    #[arg(short = 'n', long = "line-number", action = ArgAction::SetTrue, global = true)]
    line_number: bool,
    /// Only print paths with matches (one bare path per line)
    #[arg(short = 'l', long = "files-with-matches", global = true)]
    files_with_matches: bool,
    /// Only print the count of matching lines per file (`path:count`)
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
    /// Sort output: `path` (for -l, -c and --budget 0); other kinds are ignored
    #[arg(long = "sort", value_name = "KIND", global = true)]
    sort: Option<String>,

    // ---- ripgrep flags accepted for compatibility and ignored ----
    #[arg(short = 'N', long = "no-line-number", global = true, hide = true)]
    _no_line_number: bool,
    #[arg(short = 'H', long = "with-filename", global = true, hide = true)]
    _with_filename: bool,
    #[arg(short = 'h', long = "no-filename", global = true, hide = true)]
    _no_filename: bool,
    #[arg(long = "heading", global = true, hide = true)]
    _heading: bool,
    #[arg(long = "no-heading", global = true, hide = true)]
    _no_heading: bool,
    #[arg(long = "color", value_name = "WHEN", global = true, hide = true)]
    _color: Option<String>,
    #[arg(long = "colors", value_name = "SPEC", global = true, hide = true)]
    _colors: Vec<String>,
    #[arg(short = 'p', long = "pretty", global = true, hide = true)]
    _pretty: bool,
    #[arg(long = "column", global = true, hide = true)]
    _column: bool,
    #[arg(long = "no-column", global = true, hide = true)]
    _no_column: bool,
    #[arg(long = "sortr", value_name = "KIND", global = true, hide = true)]
    sortr: Option<String>,
    #[arg(long = "no-messages", global = true, hide = true)]
    _no_messages: bool,
    #[arg(long = "trim", global = true, hide = true)]
    _trim: bool,
    #[arg(long = "line-buffered", global = true, hide = true)]
    _line_buffered: bool,
    #[arg(long = "block-buffered", global = true, hide = true)]
    _block_buffered: bool,
    /// -u: --no-ignore; -uu: --no-ignore --hidden. `-uuu` also means `--text`
    /// in ripgrep, which greeg rejects rather than answers wrongly (see `text`).
    #[arg(short = 'u', long = "unrestricted", action = ArgAction::Count, global = true, hide = true)]
    unrestricted: u8,
    /// Search binary files as text. greeg's index never holds them and its
    /// scan mode stops at the first NUL, so accepting this flag would drop
    /// matches ripgrep reports. Rejected with exit 2 instead of ignored.
    #[arg(short = 'a', long = "text", global = true, hide = true)]
    text: bool,
    #[arg(long = "no-config", global = true, hide = true)]
    _no_config: bool,

    // ---- greeg ----
    /// Output token budget (0 = unlimited, rg-shaped `path:line:text` in path order)
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
    /// Matching policy: exact (no relaxation) or discover (retry empty answers).
    /// Default: exact for -l, -c, --mode files|count, --budget 0 and --json; discover for ranked text.
    #[arg(long, value_parser = ["exact", "discover"], global = true)]
    matching: Option<String>,
    /// Legacy spelling of --matching exact (conflicts with --matching)
    #[arg(long = "no-ladder", conflicts_with = "matching", global = true)]
    no_ladder: bool,
    /// Maximum hits shown per file
    #[arg(long = "per-file", default_value_t = 4, global = true)]
    per_file: usize,
    /// Show the full enclosing chain (`Outer › Inner › fn`) instead of the last container
    #[arg(long = "chain", global = true)]
    chain: bool,
    /// Root directory paths are reported relative to
    #[arg(long = "root", global = true)]
    root: Option<PathBuf>,
    /// Print scan statistics to stderr and timings in the footer
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
        /// Publish pending changes as a delta (or rebuild past the threshold): the
        /// detached step of a search that answered first
        #[arg(long = "refresh")]
        refresh: bool,
    },
    /// Where is NAME defined? Ranked by kind, visibility, PageRank and reachability from --from; --mode block prints each body
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
    /// The definition enclosing FILE:LINE, whole and dedented (20 lines each way when nothing encloses it)
    Show {
        /// One or more `path:line` (a trailing `:column` is ignored)
        #[arg(value_name = "FILE:LINE", required = true)]
        locations: Vec<String>,
    },
    /// Definitions of one file as a tree
    Outline {
        file: String,
        /// List the file's imports (shown by default only when there are at most three)
        #[arg(long = "imports")]
        imports: bool,
    },
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
    /// Agent integrations: `greeg hook claude` / `greeg hook codex` install the rg→greeg hook and the skill file
    Hook {
        #[command(subcommand)]
        which: HookCmd,
    },
    /// Runtime-loaded languages: `greeg lang check DIR` validates and measures coverage
    Lang {
        #[command(subcommand)]
        which: LangCmd,
    },
    /// Opt-in local usage stats: latency and token percentiles, savings vs the rg/grep calls the hook replaced
    Stats {
        #[command(subcommand)]
        which: Option<StatsCmd>,
        #[command(flatten)]
        filter: StatsFilter,
        /// rg output cap in bytes for the token comparison (default: `stats_cap` in the config file, else 30000, Claude Code's Bash output limit)
        #[arg(long = "cap", global = true)]
        cap: Option<usize>,
        /// List every replayed query (largest saving first) and the runs per directory (never printed otherwise)
        #[arg(long = "verbose", global = true)]
        verbose: bool,
    },
}

#[derive(Args, Debug, Clone)]
struct StatsFilter {
    /// Only records newer than this: 30m, 12h, 7d, 2w
    #[arg(long = "since", value_name = "DURATION", global = true)]
    since: Option<String>,
    /// Only records from this repository (`.` for the current one)
    #[arg(long = "repo", value_name = "PATH", global = true)]
    repo: Option<PathBuf>,
    /// Only records from one agent session: a Claude Code session id (a prefix will do), or `current` for the session running this command
    #[arg(long = "session-id", value_name = "ID", global = true)]
    session_id: Option<String>,
    /// Only records made by this greeg build (`0.4.0` covers `0.4.0+<commit>` builds; `unversioned` for records older than 0.4), and its replays as the counterfactual
    #[arg(long = "greeg", value_name = "VERSION", global = true)]
    greeg: Option<String>,
}

#[derive(Subcommand, Debug)]
enum StatsCmd {
    /// Turn collection on (writes `stats = true` to ~/.config/greeg/config.toml)
    Enable,
    /// Turn collection off (records are kept)
    Disable,
    /// Whether collection is on, where the records are, how many there are
    Status,
    /// Delete every record
    Clear,
    /// What each agent session saved, newest first (the hook records the Claude Code session id; greeg runs record CLAUDE_CODE_SESSION_ID)
    Sessions,
    /// Two greeg builds side by side on the queries replayed under both (`replay --binary PATH` replays with another build)
    Compare {
        /// The older build: a version, a release family (`0.3.0`) or a build prefix (`0.4.0+ff9`)
        #[arg(value_name = "A")]
        a: String,
        /// The newer build
        #[arg(value_name = "B")]
        b: String,
    },
    /// Run the original rg/grep commands and their greeg rewrites under the same conditions
    Replay {
        /// Timed runs per command after one warm-up (median is kept)
        #[arg(long = "runs", default_value_t = 3)]
        runs: usize,
        /// Replay at most this many distinct queries (newest first)
        #[arg(long = "limit")]
        limit: Option<usize>,
        /// Re-run queries that already have a replay record
        #[arg(long = "force")]
        force: bool,
        /// Kill a replayed command after this many seconds
        #[arg(long = "timeout", default_value_t = 60)]
        timeout: u64,
        /// Replay with this greeg binary instead of the running one (it gets an index directory of its own under the stats cache)
        #[arg(long = "binary", value_name = "PATH")]
        binary: Option<PathBuf>,
        /// Replay rg commands with this ripgrep (absolute path; default: the first rg on an absolute PATH entry)
        #[arg(long = "rg", value_name = "PATH")]
        rg: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum HookCmd {
    /// Claude Code: a PreToolUse hook in ~/.claude/settings.json that rewrites supported simple `rg` Bash calls to `greeg`, and ~/.claude/skills/greeg/SKILL.md
    Claude {
        /// Remove the hook and the skill file
        #[arg(long)]
        uninstall: bool,
        /// Print what would change without writing
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Codex: the same hook as `[[hooks.PreToolUse]]` in $CODEX_HOME/config.toml (default ~/.codex), and skills/greeg/SKILL.md next to it; trust it with /hooks in Codex
    Codex {
        /// Remove the hook and the skill file
        #[arg(long)]
        uninstall: bool,
        /// Print what would change without writing
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Show what the hook does with a Bash COMMAND: the greeg command it would run (exit 0), or `declined: REASON` (exit 1)
    Explain {
        /// The Bash command, quoted as one argument: `greeg hook explain 'rg -l foo src'`
        command: String,
    },
    /// The hook itself: reads the tool call on stdin, prints a rewritten command (internal)
    Run {
        /// Which agent is calling; shapes the reply (Codex applies a rewrite only with permissionDecision=allow)
        #[arg(long, value_enum, default_value_t = hook::Agent::Claude)]
        agent: hook::Agent,
    },
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
fn run_index(
    root: PathBuf,
    index_dir: Option<PathBuf>,
    status: bool,
    check: bool,
    fresh: String,
    threads: usize,
    phase1: bool,
    quiet: bool,
    refresh: bool,
) -> Result<()> {
    let dir = greeg_index::index_dir(&root, index_dir.as_deref())?;
    if refresh {
        return greeg_query::indexed::refresh_now(
            &root,
            &dir,
            if threads == 0 { 4 } else { threads },
        );
    }
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
        let mode = greeg_index::fresh::Mode::parse(&fresh)
            .ok_or_else(|| anyhow::anyhow!("bad --fresh"))?;
        let t = std::time::Instant::now();
        match greeg_index::fresh::check(&idx, &root, mode, 4) {
            Some(ch) => {
                println!(
                    "{}: modified {} deleted {} added {} added_dirs {} touched_dirs {} ignore_changed {} in {:.1} ms",
                    ch.method,
                    ch.modified.len(),
                    ch.deleted.len(),
                    ch.added.len(),
                    ch.added_dirs.len(),
                    ch.touched_dirs.len(),
                    ch.ignore_changed,
                    ch.ms
                );
                if greeg_index::fresh::needs_rebuild(&idx, &ch) {
                    println!("needs full rebuild");
                } else {
                    let n = greeg_index::fresh::apply(&idx, &root, &ch)?;
                    println!(
                        "applied delta with {n} files in {:.1} ms total",
                        t.elapsed().as_secs_f64() * 1e3
                    );
                }
            }
            None => println!("skipped (ttl or mode none)"),
        }
        return Ok(());
    }
    greeg_index::create_private_dir(&dir)?;
    let marker = dir.join("BUILDING");
    match greeg_index::private_file()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(mut f) => {
            use std::io::Write as _;
            let _ = greeg_index::owner_only(&f);
            let _ = writeln!(f, "{}", std::process::id());
        }
        Err(_) => {
            // stale marker (older than 10 min) is taken over
            let stale = std::fs::metadata(&marker)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|m| m.elapsed().ok())
                .map(|e| e.as_secs() > 600)
                .unwrap_or(true);
            if !stale {
                if !quiet {
                    eprintln!(
                        "greeg index: another build is running ({})",
                        marker.display()
                    );
                }
                return Ok(());
            }
        }
    }
    let mut opts = greeg_index::build::BuildOpts {
        quiet,
        phase1_only: phase1,
        ..Default::default()
    };
    if threads > 0 {
        opts.reader_threads = threads;
    }
    let r = greeg_index::build::build(&root, &dir, &opts);
    let _ = std::fs::remove_file(&marker);
    let m = r?;
    if !quiet {
        eprintln!(
            "greeg index: written to {} (generation {})",
            dir.display(),
            m.generation
        );
    }
    Ok(())
}

struct Argv(Vec<*const libc::c_char>);
unsafe impl Sync for Argv {}
unsafe impl Send for Argv {}
static REEXEC: std::sync::OnceLock<(Vec<std::ffi::CString>, Argv)> = std::sync::OnceLock::new();

/// SIGBUS means an mmapped index file was truncated underneath us. Re-exec the
/// same command with `--no-index --after-sigbus` (execv is async-signal-safe):
/// the answer comes from a scan and the index is rebuilt. The flags go first,
/// where no `--` can turn them into patterns or paths.
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
    let flags = args.iter().skip(1).take_while(|a| *a != "--");
    if flags.clone().any(|a| a == "--after-sigbus") || flags.clone().any(|a| a == "--no-index") {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    use std::os::unix::ffi::OsStrExt;
    let mut cs: Vec<std::ffi::CString> = Vec::with_capacity(args.len() + 3);
    let Ok(e) = std::ffi::CString::new(exe.as_os_str().as_bytes()) else {
        return;
    };
    cs.push(e);
    cs.push(std::ffi::CString::new("--no-index").unwrap());
    cs.push(std::ffi::CString::new("--after-sigbus").unwrap());
    for a in args.iter().skip(1) {
        // an argument holding NUL cannot be passed on: without it the re-run
        // would answer a different query
        let Ok(c) = std::ffi::CString::new(a.as_bytes()) else {
            return;
        };
        cs.push(c);
    }
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
    // catch_unwind and degrades to a scan.
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let loc = info
            .location()
            .map(|l| format!(" at {}:{}", l.file(), l.line()))
            .unwrap_or_default();
        eprintln!("greeg: internal error{loc}: {msg}");
    }));
    install_sigbus_guard();
    run()
}

fn build_options(c: &Common, pattern: String, paths: Vec<PathBuf>) -> Result<Options> {
    // A flag greeg cannot honour is an error, not a silent divergence: the
    // answer would be missing the matches the flag was asked for. Cosmetic
    // flags (-N -H --color --no-heading --column ...) stay accepted and
    // ignored, because the output they ask for is the output greeg gives.
    if c.text {
        anyhow::bail!(
            "greeg does not search binary files (-a/--text): its index holds no binary file \
             and its scan stops at the first NUL, so the answer would be missing matches. \
             Use `rg -a` for this one."
        );
    }
    if c.unrestricted >= 3 {
        anyhow::bail!(
            "`-uuu` means `-uu --text` in ripgrep and greeg does not search binary files. \
             Use `-uu` for ignored and hidden files, or `rg -uuu` for this one."
        );
    }
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
        Some(k) => k
            .split(',')
            .map(|s| {
                HitKind::parse(s.trim()).ok_or_else(|| anyhow::anyhow!("unknown --kind {s:?}"))
            })
            .collect::<Result<Vec<_>>>()?,
        None => vec![],
    };
    let sort_path = match c.sort.as_deref().or(c.sortr.as_deref()) {
        None => false,
        Some("path") => true,
        Some(other) => {
            if !c._no_messages {
                eprintln!("greeg: --sort {other} is not supported (only `path`); ignored");
            }
            false
        }
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
        no_ignore: c.no_ignore || c.unrestricted >= 1,
        hidden: c.hidden || c.unrestricted >= 2,
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
        matching: match c.matching.as_deref() {
            Some("discover") => MatchingPolicy::Discover,
            Some("exact") => MatchingPolicy::Exact,
            _ if c.no_ladder
                || c.json
                || c.budget == 0
                || matches!(mode, Mode::Files | Mode::Count) =>
            {
                MatchingPolicy::Exact
            }
            _ => MatchingPolicy::Discover,
        },
        max_columns: c.max_columns,
        per_file_cap: c.per_file,
        context: c.context,
        before: c.before.unwrap_or(0),
        after: c.after.unwrap_or(0),
        use_index: !c.no_index,
        fresh: greeg_index::fresh::Mode::parse(&c.fresh).ok_or_else(|| {
            anyhow::anyhow!("unknown --fresh {:?} (auto|none|stat|fsevents)", c.fresh)
        })?,
        index_dir: c.index_dir.clone(),
        precise: c.precise,
        sort_path,
    })
}

/// Join `-e` patterns the way ripgrep does: each becomes an alternative; with
/// `-F` each is escaped and the joined pattern is a regex.
fn join_patterns(pats: &[String], fixed: bool) -> (String, bool) {
    if pats.len() == 1 {
        return (pats[0].clone(), fixed);
    }
    let alts: Vec<String> = pats
        .iter()
        .map(|p| format!("(?:{})", if fixed { regex_escape(p) } else { p.clone() }))
        .collect();
    (alts.join("|"), false)
}

fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if !c.is_alphanumeric() && c != '_' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Rendering choices that live outside `Options`.
#[derive(Clone, Copy)]
pub(crate) struct Fmt {
    pub(crate) chain: bool,
    pub(crate) stats: bool,
    /// `-n` was given (line numbers for stdin, as ripgrep).
    pub(crate) line_numbers: bool,
    pub(crate) stdin: bool,
}

fn run() -> Result<()> {
    let trace = std::env::var_os("GREEG_DEBUG_START").is_some();
    if trace {
        eprintln!(
            "greeg: main entry at {:?} µs after process start",
            stats::since_start_us()
        );
    }
    let cli = Cli::parse();
    if trace {
        eprintln!("greeg: args parsed at {:?} µs", stats::since_start_us());
    }
    let c = &cli.common;
    stats::begin();
    if let Some(cmd) = cli.cmd {
        let verb = match &cmd {
            Cmd::Def { .. } => "def",
            Cmd::Refs { .. } => "refs",
            Cmd::Callers { .. } => "callers",
            Cmd::Impls { .. } => "impls",
            Cmd::Outline { .. } => "outline",
            Cmd::Show { .. } => "show",
            Cmd::Map { .. } => "map",
            Cmd::Impact { .. } => "impact",
            _ => "",
        };
        let r = match cmd {
            Cmd::Index {
                root,
                index_dir,
                status,
                check,
                fresh,
                threads,
                phase1,
                quiet,
                refresh,
            } => run_index(
                root, index_dir, status, check, fresh, threads, phase1, quiet, refresh,
            ),
            Cmd::Def {
                name,
                from,
                def_kind,
            } => verbs_out::run_def(
                c,
                &build_options(c, name.clone(), vec![])?,
                &name,
                &from,
                def_kind.as_deref(),
            ),
            Cmd::Refs { name } => {
                verbs_out::run_refs(c, &build_options(c, name.clone(), vec![])?, &name)
            }
            Cmd::Callers { name, depth } => {
                verbs_out::run_callers(c, &build_options(c, name.clone(), vec![])?, &name, depth)
            }
            Cmd::Impls { name } => {
                verbs_out::run_impls(c, &build_options(c, name.clone(), vec![])?, &name)
            }
            Cmd::Outline { file, imports } => {
                verbs_out::run_outline(c, &build_options(c, String::new(), vec![])?, &file, imports)
            }
            Cmd::Show { locations } => {
                let locs = locations
                    .iter()
                    .map(|s| parse_location(s))
                    .collect::<Result<Vec<_>>>()?;
                verbs_out::run_show(c, &build_options(c, String::new(), vec![])?, &locs)
            }
            Cmd::Map { dir } => {
                verbs_out::run_map(c, &build_options(c, String::new(), vec![])?, &dir)
            }
            Cmd::Impact { name } => {
                verbs_out::run_impact(c, &build_options(c, name.clone(), vec![])?, &name)
            }
            Cmd::Doctor => doctor::run(c),
            Cmd::Man => {
                let mut out = Vec::new();
                clap_mangen::Man::new(Cli::command()).render(&mut out)?;
                std::io::stdout().write_all(&out)?;
                Ok(())
            }
            Cmd::Hook {
                which: HookCmd::Claude { uninstall, dry_run },
            } => hook::install_claude(uninstall, dry_run),
            Cmd::Hook {
                which: HookCmd::Codex { uninstall, dry_run },
            } => hook::install_codex(uninstall, dry_run),
            Cmd::Hook {
                which: HookCmd::Explain { command },
            } => {
                if !hook::explain(&command)? {
                    std::process::exit(1);
                }
                Ok(())
            }
            Cmd::Hook {
                which: HookCmd::Run { agent },
            } => hook::run(agent),
            Cmd::Lang {
                which: LangCmd::Check { dir },
            } => doctor::lang_check(&dir),
            Cmd::Stats {
                which,
                filter,
                cap,
                verbose,
            } => run_stats(c, which, filter, cap, verbose),
        };
        if !verb.is_empty() && r.is_ok() {
            stats::record_run(stats::RunInfo {
                verb,
                ..Default::default()
            });
        }
        return r;
    }
    if c.after_sigbus {
        // the previous process died on a truncated index file: the index is unusable
        eprintln!(
            "greeg: an index file was truncated while in use; answering from a scan and rebuilding the index"
        );
        let o = build_options(c, String::new(), vec![])?;
        greeg_query::indexed::mark_corrupt(&o);
    }
    let (pattern, paths, fixed) = if cli.regexp.is_empty() {
        match &cli.pattern {
            Some(p) => (p.clone(), cli.paths.clone(), c.fixed_strings),
            None => anyhow::bail!("missing PATTERN (see --help)"),
        }
    } else {
        let (p, fixed) = join_patterns(&cli.regexp, c.fixed_strings);
        let paths: Vec<PathBuf> = cli
            .pattern
            .iter()
            .map(PathBuf::from)
            .chain(cli.paths.iter().cloned())
            .collect();
        (p, paths, fixed)
    };
    let mut opts = build_options(c, pattern, paths)?;
    opts.fixed_strings = fixed;
    let fmt = Fmt {
        chain: c.chain,
        stats: c.stats,
        line_numbers: c.line_number,
        stdin: false,
    };
    if opts.paths.is_empty() && greeg_query::stdin::is_readable_stdin() && !stdin_is_socket() {
        return run_stdin(c, opts, fmt);
    }
    if opts.sort_path
        && opts.budget > 0
        && !matches!(opts.mode, Mode::Files | Mode::Count)
        && !c._no_messages
    {
        eprintln!(
            "greeg: --sort path applies to -l, -c and --budget 0; ranked output keeps score order"
        );
    }
    let session = if c.no_session {
        None
    } else {
        greeg_query::session::Session::open(&opts, c.session.as_deref())
    };
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
    emit(c, &result, &report, fmt)?;
    if let Some(s) = &session {
        s.record(&opts, &result, &report);
    }
    if c.stats {
        let s = &result.stats;
        let t_render = t0.elapsed() - t_scan - t_shape;
        eprintln!(
            "greeg: {} · walked {} files, candidates {}, searched {}, matched {} ({} hits) with {} threads; binary {} huge {}",
            s.source,
            s.files_walked,
            s.candidates,
            s.files_searched,
            s.files_matched,
            s.total_hits,
            s.threads,
            s.skipped_binary,
            s.skipped_huge
        );
        if s.source == "index" {
            eprintln!(
                "greeg: fresh {} {:.1} ms ({} changed{}) · plan {}",
                s.fresh_method,
                s.fresh_ms,
                s.fresh_changed,
                if s.fresh_deferred > 0 {
                    ", delta after the answer"
                } else {
                    ""
                },
                if s.plan.len() > 120 {
                    format!("{}…", &s.plan[..120])
                } else {
                    s.plan.clone()
                }
            );
        }
        eprintln!(
            "greeg: scan {:.1} ms · shape {:.1} ms (refine {:.1}) · render {:.1} ms · total {:.1} ms · cpu: read {:.1} search {:.1} classify {:.1} ms",
            t_scan.as_secs_f64() * 1e3,
            t_shape.as_secs_f64() * 1e3,
            s.refine_ms,
            t_render.as_secs_f64() * 1e3,
            t0.elapsed().as_secs_f64() * 1e3,
            s.cpu_read_ms,
            s.cpu_search_ms,
            s.cpu_classify_ms
        );
    }
    verbs_out::finish_run(
        stats::RunInfo {
            verb: "search",
            scan_ms: Some(t_scan.as_secs_f64() * 1e3),
            shape_ms: Some(t_shape.as_secs_f64() * 1e3),
            source: Some(result.stats.source.to_string()),
            hits: Some(result.stats.total_hits),
            files: Some(result.stats.files_matched),
            exit: 0,
        },
        &Outcome::of_search(&result, report.footer.hits_shown),
    )
}

fn run_stats(
    c: &Common,
    which: Option<StatsCmd>,
    f: StatsFilter,
    cap: Option<usize>,
    verbose: bool,
) -> Result<()> {
    let session = match f.session_id.as_deref() {
        Some("current") => Some(stats::agent_session().ok_or_else(|| {
            anyhow::anyhow!(
                "no agent session in the environment (CLAUDE_CODE_SESSION_ID or GREEG_SESSION)"
            )
        })?),
        Some(id) => Some(id.to_string()),
        None => None,
    };
    let filter = stats::Filter {
        since_ms: f.since.as_deref().map(stats::parse_since).transpose()?,
        repo: f.repo,
        session,
        greeg: f.greeg,
    };
    match which {
        None => stats::report(&stats::ReportOpts {
            filter,
            cap,
            json: c.json,
            verbose,
        }),
        Some(StatsCmd::Enable) => stats::enable(cap),
        Some(StatsCmd::Disable) => stats::disable(),
        Some(StatsCmd::Status) => stats::status(),
        Some(StatsCmd::Clear) => stats::clear(),
        Some(StatsCmd::Sessions) => stats::sessions(&stats::ReportOpts {
            filter,
            cap,
            json: c.json,
            verbose,
        }),
        Some(StatsCmd::Compare { a, b }) => stats::compare(
            &stats::ReportOpts {
                filter,
                cap,
                json: c.json,
                verbose,
            },
            &a,
            &b,
        ),
        Some(StatsCmd::Replay {
            runs,
            limit,
            force,
            timeout,
            binary,
            rg,
        }) => stats::replay(&stats::ReplayOpts {
            filter,
            runs,
            limit,
            force,
            timeout: std::time::Duration::from_secs(timeout),
            binary,
            rg,
        }),
    }
}

/// Some agent harnesses attach a Unix socket as stdin and never close it:
/// ripgrep would wait on it forever, and so would we. A socket is not a pipe
/// or a file with data, so with no path given the tree is searched instead.
fn stdin_is_socket() -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata("/dev/stdin").is_ok_and(|m| m.file_type().is_socket())
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// `path:line` or `path:line:column` (the column is ignored); a path may
/// itself contain colons.
fn parse_location(s: &str) -> Result<(String, u32)> {
    let mut parts = s.rsplitn(3, ':');
    let last = parts.next().unwrap_or("");
    let mid = parts.next();
    let rest = parts.next();
    if let (Ok(_col), Some(m), Some(r)) = (last.parse::<u32>(), mid, rest)
        && let Ok(line) = m.parse::<u32>()
    {
        return Ok((r.to_string(), line));
    }
    if let (Ok(line), Some(m)) = (last.parse::<u32>(), mid) {
        let path = match rest {
            Some(r) => format!("{r}:{m}"),
            None => m.to_string(),
        };
        return Ok((path, line));
    }
    anyhow::bail!("show needs FILE:LINE, got {s:?}")
}

/// C6: no path given and stdin is a pipe or file: search it like ripgrep.
fn run_stdin(c: &Common, mut opts: Options, fmt: Fmt) -> Result<()> {
    use std::io::Read;
    if c.matching.as_deref() == Some("discover") {
        anyhow::bail!(
            "--matching discover requires file input; stdin supports exact matching only"
        );
    }
    opts.budget = 0;
    opts.matching = MatchingPolicy::Exact;
    opts.use_index = false;
    opts.max_columns = 0;
    let mut data = Vec::new();
    std::io::stdin().lock().read_to_end(&mut data)?;
    let mut result = greeg_query::stdin::scan(&opts, data)?;
    let report = greeg_query::shape::shape(&mut result);
    emit(c, &result, &report, Fmt { stdin: true, ..fmt })?;
    verbs_out::finish_run(
        stats::RunInfo {
            verb: "search",
            hits: Some(result.stats.total_hits),
            ..Default::default()
        },
        &Outcome::of_search(&result, report.footer.hits_shown),
    )
}

/// Write the answer: JSON records, or the text body plus the footer (stdout;
/// stderr for `-l`/`-c` and `--budget 0`, whose stdout stays pipe-safe).
fn emit(c: &Common, result: &ScanResult, report: &Report, fmt: Fmt) -> Result<()> {
    let stdout = std::io::stdout();
    let mut w = BufWriter::with_capacity(64 * 1024, stats::Tee(stdout.lock()));
    if c.json {
        render_json(&mut w, result, report)?;
        w.flush()?;
        return Ok(());
    }
    let mut body: Vec<u8> = Vec::with_capacity(16 * 1024);
    render_body(&mut body, result, report, fmt)?;
    w.write_all(&body)?;
    let pipe_safe =
        matches!(report.layout, Layout::Files | Layout::Count) || result.opts.budget == 0;
    if pipe_safe {
        w.flush()?;
        if !fmt.stdin {
            let mut f = Vec::new();
            render_footer(&mut f, result, report, report.footer.est_tokens, fmt, false)?;
            std::io::stderr().write_all(&f)?;
            stats::observe(&f);
        }
    } else {
        // measured estimate for the whole output: body plus the footer itself
        let mut probe = Vec::new();
        render_footer(&mut probe, result, report, 8888, fmt, true)?;
        let est = greeg_query::tokens::rendered(&body) + greeg_query::tokens::rendered(&probe);
        render_footer(&mut w, result, report, est, fmt, true)?;
        w.flush()?;
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

pub(crate) fn chain_str(chain: &[(greeg_lang::DefKind, String)]) -> String {
    chain
        .iter()
        .map(|(_, n)| n.as_str())
        .collect::<Vec<_>>()
        .join(" › ")
}

/// Container to display for a hit: the last enclosing `Class.method` (a
/// definition's own name is already in its text); the full chain with `--chain`.
pub(crate) fn container_of(
    chain: &[(greeg_lang::DefKind, String)],
    is_def: bool,
    full: bool,
) -> String {
    let chain = if is_def && !chain.is_empty() {
        &chain[..chain.len() - 1]
    } else {
        chain
    };
    if full {
        chain_str(chain)
    } else {
        let n = chain.len();
        chain[n.saturating_sub(2)..]
            .iter()
            .map(|(_, s)| s.as_str())
            .collect::<Vec<_>>()
            .join(".")
    }
}

/// `[test,vendored]` suffix for a file header (mock paths count as demoted).
pub(crate) fn flag_suffix(f: &greeg_query::FileResult) -> String {
    let mut flags = f.flags.names();
    flags.retain(|x| *x != "huge");
    if flags.is_empty() && greeg_query::is_mock_path(&f.rel) {
        flags.push("mock");
    }
    if flags.is_empty() {
        String::new()
    } else {
        format!("  [{}]", flags.join(","))
    }
}

fn digits(n: u32) -> usize {
    n.max(1).ilog10() as usize + 1
}

fn text_of(h: &greeg_query::Hit) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(&h.text)
}

/// The hit sits on the definition line of its enclosing symbol: the text
/// already names it, so the container link is the parent instead.
fn own_def(f: &greeg_query::FileResult, h: &greeg_query::Hit) -> bool {
    h.def_idx
        .is_some_and(|di| f.defs.get(di as usize).is_some_and(|d| d.line == h.line))
}

/// One `  <line> <kind> <text>  ‹ container` row. `kw` is the kind column width
/// (0 = no kind column); ident hits leave the column blank. The container is
/// printed only when it differs from the previous row's, and never names the
/// symbol whose definition line the row is (`own_def`).
fn hit_row(
    h: &greeg_query::Hit,
    own_def: bool,
    lw: usize,
    kw: usize,
    fmt: Fmt,
    last: &mut String,
    extra: &str,
) -> String {
    let kind = if kw == 0 {
        String::new()
    } else if h.kind == HitKind::Ident {
        format!(" {:kw$}", "")
    } else {
        format!(" {:<kw$}", h.kind.name())
    };
    let c = container_of(&h.chain, h.kind == HitKind::Def || own_def, fmt.chain);
    let tag = if !c.is_empty() && *last != c {
        format!("  ‹ {c}")
    } else {
        String::new()
    };
    *last = c;
    format!(
        "  {:>lw$}{kind}  {}{tag}{extra}",
        h.line,
        text_of(h).trim_end()
    )
}

/// Kind column width for a set of hits: the widest non-ident kind, 0 when all are ident.
fn kind_width<'a>(hits: impl Iterator<Item = &'a greeg_query::Hit>) -> usize {
    hits.filter(|h| h.kind != HitKind::Ident)
        .map(|h| h.kind.name().len())
        .max()
        .unwrap_or(0)
}

/// Hits grouped by file (order of first appearance), one header per file.
pub(crate) fn write_groups(
    w: &mut impl Write,
    r: &ScanResult,
    list: &[(usize, usize)],
    fmt: Fmt,
    with_kind: bool,
) -> Result<()> {
    let mut order: Vec<usize> = Vec::new();
    let mut groups: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for &(fi, hi) in list {
        if !groups.contains_key(&fi) {
            order.push(fi);
        }
        groups.entry(fi).or_default().push(hi);
    }
    for fi in order {
        let f = &r.files[fi];
        let hits = groups.get_mut(&fi).unwrap();
        hits.sort_by_key(|&hi| f.hits[hi].line);
        let hits = &hits[..];
        writeln!(w, "{}{}", f.rel, flag_suffix(f))?;
        let lw = hits
            .iter()
            .map(|&hi| digits(f.hits[hi].line))
            .max()
            .unwrap_or(1);
        let kw = if with_kind {
            kind_width(hits.iter().map(|&hi| &f.hits[hi]))
        } else {
            0
        };
        let mut last = String::new();
        for &hi in hits {
            writeln!(
                w,
                "{}",
                hit_row(
                    &f.hits[hi],
                    own_def(f, &f.hits[hi]),
                    lw,
                    kw,
                    fmt,
                    &mut last,
                    ""
                )
            )?;
        }
    }
    Ok(())
}

/// Short display names for an `imported by` list; a name that would repeat
/// falls back to the full path.
pub(crate) fn short_names<'a>(rels: impl Iterator<Item = &'a str>) -> Vec<String> {
    let rels: Vec<&str> = rels.collect();
    let short: Vec<String> = rels.iter().map(|r| short_name(r)).collect();
    short
        .iter()
        .enumerate()
        .map(|(i, s)| {
            if short.iter().filter(|x| *x == s).count() > 1 {
                rels[i].to_string()
            } else {
                s.clone()
            }
        })
        .collect()
}

/// Short display name for a file in an `imported by` list.
pub(crate) fn short_name(rel: &str) -> String {
    let mut parts = rel.rsplit('/');
    let base = parts.next().unwrap_or(rel);
    let stem = base.split('.').next().unwrap_or(base);
    if matches!(stem, "mod" | "index" | "__init__" | "lib" | "main")
        && let Some(dir) = parts.next()
    {
        return format!("{dir}/{base}");
    }
    base.to_string()
}

fn render_body(w: &mut impl Write, r: &ScanResult, rep: &Report, fmt: Fmt) -> Result<()> {
    let files = &r.files;
    let parity = r.opts.budget == 0;
    match rep.layout {
        Layout::Files => {
            for sf in &rep.files {
                writeln!(w, "{}", files[sf.file].rel)?;
            }
        }
        Layout::Count => {
            for sf in &rep.files {
                if fmt.stdin {
                    writeln!(w, "{}", files[sf.file].total)?;
                } else {
                    writeln!(w, "{}:{}", files[sf.file].rel, files[sf.file].total)?;
                }
            }
        }
        Layout::Outline => {
            for sf in &rep.files {
                let f = &files[sf.file];
                writeln!(w, "{}{}", f.rel, flag_suffix(f))?;
                let lw = sf
                    .hits
                    .iter()
                    .map(|sh| digits(f.hits[sh.hit].line))
                    .max()
                    .unwrap_or(1);
                let kw = kind_width(sf.hits.iter().map(|sh| &f.hits[sh.hit]));
                for sh in &sf.hits {
                    let h = &f.hits[sh.hit];
                    let kind = if kw == 0 {
                        String::new()
                    } else if h.kind == HitKind::Ident {
                        format!(" {:kw$}", "")
                    } else {
                        format!(" {:<kw$}", h.kind.name())
                    };
                    let ch = container_of(&h.chain, h.kind == HitKind::Def, fmt.chain);
                    writeln!(
                        w,
                        "  {:>lw$}{kind}  {}{}{}",
                        h.line,
                        ch,
                        if ch.is_empty() { "" } else { " › " },
                        text_of(h).trim_end()
                    )?;
                }
                if sf.more > 0 {
                    writeln!(w, "  +{} more", sf.more)?;
                }
            }
        }
        Layout::Content if parity => render_parity(w, r, rep, fmt)?,
        Layout::Content | Layout::Block => {
            let mut first = true;
            for sf in &rep.files {
                let f = &files[sf.file];
                if !first {
                    writeln!(w)?;
                }
                first = false;
                writeln!(w, "{}{}", f.rel, flag_suffix(f))?;
                render_hits(w, f, sf, fmt)?;
            }
        }
        Layout::Facets => {
            let fc = rep.facets.as_ref().unwrap();
            if fc.defs_total > 0 {
                let shown = fc.top_defs.len();
                writeln!(w, "definitions ({} of {})", shown, fc.defs_total)?;
                write_groups(w, r, &fc.top_defs, fmt, false)?;
                for (g, n) in &fc.demoted_defs {
                    writeln!(w, "  +{n} {g} definitions (--all)")?;
                }
                writeln!(w)?;
            }
            let kinds: Vec<String> = fc
                .by_kind
                .iter()
                .map(|(k, n)| format!("{} {}", k.name(), fmt_n(*n)))
                .collect();
            let flags: Vec<String> = fc
                .by_flag
                .iter()
                .map(|(l, n)| format!("{} {}", l, fmt_n(*n)))
                .collect();
            write!(
                w,
                "{}  {} hits · {} files · {}",
                r.opts.pattern,
                fmt_n(r.stats.total_hits),
                fmt_n(r.stats.files_matched),
                kinds.join(" ")
            )?;
            if !flags.is_empty() {
                write!(w, " · {}", flags.join(" "))?;
            }
            writeln!(w)?;
            let dirs: Vec<String> = fc
                .by_dir
                .iter()
                .map(|(d, n)| format!("{} {}", d, fmt_n(*n)))
                .collect();
            writeln!(w, "areas  {}", dirs.join("  "))?;
            if !fc.by_lang.is_empty() {
                let langs: Vec<String> = fc
                    .by_lang
                    .iter()
                    .map(|(l, n)| format!("{} {}", l, fmt_n(*n)))
                    .collect();
                writeln!(w, "langs  {}", langs.join("  "))?;
            }
            if !fc.top_hits.is_empty() || !fc.import_files.is_empty() {
                writeln!(w, "\ntop hits")?;
                if !fc.import_files.is_empty() {
                    let names = short_names(
                        fc.import_files
                            .iter()
                            .take(6)
                            .map(|&fi| files[fi].rel.as_str()),
                    );
                    let more = fc.import_files.len().saturating_sub(6);
                    writeln!(
                        w,
                        "imported by {} file{}: {}{}",
                        fc.import_files.len(),
                        if fc.import_files.len() == 1 { "" } else { "s" },
                        names.join(", "),
                        if more > 0 {
                            format!(" (+{more})")
                        } else {
                            String::new()
                        }
                    )?;
                }
                write_groups(w, r, &fc.top_hits, fmt, true)?;
            }
        }
    }
    if !rep.related.is_empty() {
        let names: Vec<String> = rep
            .related
            .iter()
            .map(|(n, c)| format!("{} {}", n, fmt_n(*c)))
            .collect();
        writeln!(w, "related  {}", names.join("  "))?;
    }
    Ok(())
}

/// `--budget 0` (and stdin): ripgrep's shape, `path:line:text` in path order,
/// context lines as `path-line-text` with `--` between groups.
fn render_parity(w: &mut impl Write, r: &ScanResult, rep: &Report, fmt: Fmt) -> Result<()> {
    let mut first_file = true;
    for sf in &rep.files {
        let f = &r.files[sf.file];
        let prefix = |ln: u32, sep: char| -> String {
            let mut s = String::new();
            if !fmt.stdin {
                s.push_str(&f.rel);
                s.push(sep);
            }
            if !fmt.stdin || fmt.line_numbers {
                s.push_str(&ln.to_string());
                s.push(sep);
            }
            s
        };
        let has_ctx = sf.hits.iter().any(|sh| sh.context.is_some());
        if has_ctx && !first_file {
            writeln!(w, "--")?;
        }
        first_file = false;
        let hit_lines: std::collections::BTreeSet<u32> =
            sf.hits.iter().map(|sh| f.hits[sh.hit].line).collect();
        let mut last = 0u32;
        for sh in &sf.hits {
            let h = &f.hits[sh.hit];
            match &sh.context {
                Some((first, lines)) => {
                    if last > 0 && *first > last + 1 {
                        writeln!(w, "--")?;
                    }
                    for (i, l) in lines.iter().enumerate() {
                        let ln = first + i as u32;
                        if ln <= last {
                            continue;
                        }
                        if hit_lines.contains(&ln) {
                            writeln!(w, "{}{}", prefix(ln, ':'), String::from_utf8_lossy(l))?;
                        } else {
                            writeln!(w, "{}{}", prefix(ln, '-'), String::from_utf8_lossy(l))?;
                        }
                        last = ln;
                    }
                }
                None => {
                    if h.line > last {
                        writeln!(
                            w,
                            "{}{}",
                            prefix(h.line, ':'),
                            String::from_utf8_lossy(&h.raw)
                        )?;
                        last = h.line;
                    }
                }
            }
        }
    }
    Ok(())
}

fn render_footer(
    w: &mut impl Write,
    r: &ScanResult,
    rep: &Report,
    est: usize,
    fmt: Fmt,
    with_hints: bool,
) -> Result<()> {
    let ft = &rep.footer;
    if with_hints {
        writeln!(w)?;
    }
    // Everything shown, nothing skipped, matched as asked: the two counts say
    // the answer is whole (and was not cut by the caller's output limit); the
    // demotion note and the token estimate only matter when something was
    // left out.
    let complete = r.stats.total_hits > 0
        && ft.hits_shown == ft.hits_total
        && ft.files_shown == ft.files_total
        && ft.skipped_binary + ft.skipped_huge == 0
        && ft.rung == greeg_query::Rung::Exact;
    if r.stats.total_hits == 0 {
        write!(w, "no hits")?;
    } else if complete {
        write!(
            w,
            "{} hits · {} files",
            fmt_n(ft.hits_total),
            fmt_n(ft.files_total)
        )?;
    } else {
        write!(
            w,
            "{}/{} hits · {}/{} files",
            fmt_n(ft.hits_shown),
            fmt_n(ft.hits_total),
            fmt_n(ft.files_shown),
            fmt_n(ft.files_total)
        )?;
        if ft.demoted_files > 0 {
            write!(
                w,
                " · {} files demoted ({})",
                ft.demoted_files,
                fmt_n(ft.demoted_hits)
            )?;
        }
    }
    // `skipped` only when non-zero, term by term: `skipped 1 binary, 0 huge`
    // spends five tokens saying nothing
    if ft.skipped_binary + ft.skipped_huge > 0 {
        let mut parts: Vec<String> = Vec::with_capacity(2);
        if ft.skipped_binary > 0 {
            parts.push(format!("{} binary", ft.skipped_binary));
        }
        if ft.skipped_huge > 0 {
            parts.push(format!("{} huge", ft.skipped_huge));
        }
        write!(w, " · skipped {}", parts.join(", "))?;
    }
    if ft.rung != greeg_query::Rung::Exact {
        write!(w, " · matched {}", ft.rung.describe())?;
    }
    if !complete && r.stats.total_hits > 0 {
        write!(w, " · ~{est} tokens")?;
    }
    if fmt.stats {
        write!(w, " · {:.0} ms", ft.elapsed_ms)?;
    }
    writeln!(w)?;
    if with_hints && !ft.hints.is_empty() {
        writeln!(w, "next: {}", ft.hints.join(" | "))?;
    }
    Ok(())
}

fn render_hits(
    w: &mut impl Write,
    f: &greeg_query::FileResult,
    sf: &ShownFile,
    fmt: Fmt,
) -> Result<()> {
    let mut max_line = 1u32;
    for sh in &sf.hits {
        max_line = max_line.max(f.hits[sh.hit].line);
        if let Some((first, lines)) = &sh.context {
            max_line = max_line.max(first + lines.len() as u32);
        }
        if let Some((first, lines)) = &sh.block {
            max_line = max_line.max(first + lines.len() as u32);
        }
    }
    let lw = digits(max_line);
    let kw = kind_width(sf.hits.iter().map(|sh| &f.hits[sh.hit]));
    let pad = if kw == 0 {
        String::new()
    } else {
        format!(" {:kw$}", "")
    };
    let mut last_line_printed = 0u32;
    let mut last = String::new();
    for sh in &sf.hits {
        let h = &f.hits[sh.hit];
        // context and bodies are dedented by the hit line's own indentation
        let indent = h
            .raw
            .iter()
            .position(|b| !matches!(b, b' ' | b'\t'))
            .unwrap_or(h.raw.len());
        let dedent = |l: &[u8]| -> String {
            let lead = l
                .iter()
                .position(|b| !matches!(b, b' ' | b'\t'))
                .unwrap_or(l.len());
            String::from_utf8_lossy(&l[lead.min(indent)..]).into_owned()
        };
        if let Some((first, lines)) = &sh.block {
            writeln!(
                w,
                "  ── {} {} (lines {}–{})",
                h.chain.last().map(|(k, _)| k.name()).unwrap_or("block"),
                chain_str(&h.chain),
                first,
                first + lines.len() as u32 - 1
            )?;
            for (i, l) in lines.iter().enumerate() {
                writeln!(w, "  {:>lw$}  {}", first + i as u32, dedent(l))?;
            }
            if sh.block_clipped {
                writeln!(w, "  … body clipped (--budget)")?;
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
                    writeln!(
                        w,
                        "{}",
                        hit_row(h, own_def(f, h), lw, kw, fmt, &mut last, "")
                    )?;
                } else {
                    writeln!(w, "  {:>lw$}{pad}  {}", ln, dedent(l))?;
                }
                last_line_printed = ln;
            }
        } else {
            let seen = if sh.seen_before {
                "  (shown before)"
            } else {
                ""
            };
            writeln!(
                w,
                "{}",
                hit_row(h, own_def(f, h), lw, kw, fmt, &mut last, seen)
            )?;
            last_line_printed = h.line;
        }
    }
    if sf.more > 0 {
        let shown: Vec<HitKind> = sf.hits.iter().map(|sh| f.hits[sh.hit].kind).collect();
        let mut rest = f.kinds;
        for k in shown {
            rest[k.idx()] = rest[k.idx()].saturating_sub(1);
        }
        let f_kinds: Vec<String> = HitKind::ALL
            .iter()
            .filter(|k| rest[k.idx()] > 0)
            .map(|k| format!("{} {}", rest[k.idx()], k.name()))
            .collect();
        let counted: u32 = rest.iter().sum();
        let unclassified = sf.more.saturating_sub(counted as usize);
        let mut parts = f_kinds;
        if unclassified > 0 && !parts.is_empty() {
            parts.push(format!("{unclassified} uncounted"));
        }
        if parts.is_empty() {
            writeln!(w, "  +{} more", sf.more)?;
        } else {
            writeln!(w, "  +{} more ({})", sf.more, parts.join(", "))?;
        }
    }
    Ok(())
}

/// Byte range (start, end including the terminator) of line `n` in `bytes`,
/// found from a known (line, line_start) anchor.
fn line_span(bytes: &[u8], anchor_line: u32, anchor_start: u32, n: u32) -> Option<(usize, usize)> {
    let mut start = anchor_start as usize;
    if n < anchor_line {
        for _ in 0..(anchor_line - n) {
            let prev = start.checked_sub(1)?; // the '\n' ending the previous line
            start = memchr::memrchr(b'\n', &bytes[..prev])
                .map(|k| k + 1)
                .unwrap_or(0);
        }
    } else {
        for _ in 0..(n - anchor_line) {
            start = memchr::memchr(b'\n', &bytes[start..]).map(|k| start + k + 1)?;
            if start >= bytes.len() {
                return None;
            }
        }
    }
    let end = memchr::memchr(b'\n', &bytes[start..])
        .map(|k| start + k + 1)
        .unwrap_or(bytes.len());
    Some((start, end))
}

fn render_json(w: &mut impl Write, r: &ScanResult, rep: &Report) -> Result<()> {
    use serde_json::json;
    let files = &r.files;
    let t0 = std::time::Instant::now();
    let mut printed_lines = 0usize;
    let mut printed_matches = 0usize;
    let mut printed_bytes = 0usize;
    let mut emit_file = |w: &mut dyn Write, sf: &ShownFile| -> Result<()> {
        let f = &files[sf.file];
        serde_json::to_writer(
            &mut *w,
            &json!({"type":"begin","data":{"path":{"text":f.rel}}}),
        )?;
        writeln!(w)?;
        // hits in line order, with their explicit context ranges merged
        let mut hits: Vec<&greeg_query::shape::ShownHit> = sf.hits.iter().collect();
        hits.sort_by_key(|sh| f.hits[sh.hit].line);
        let src = f.src.as_ref().map(|s| &s.bytes[..]);
        let hit_lines: std::collections::BTreeMap<u32, usize> = sf
            .hits
            .iter()
            .map(|sh| (f.hits[sh.hit].line, sh.hit))
            .collect();
        let mut last = 0u32;
        let mut file_matches = 0usize;
        let mut file_lines = 0usize;
        let mut emit_match = |w: &mut dyn Write, hi: usize| -> Result<()> {
            let h = &f.hits[hi];
            let text: String = match src.and_then(|b| {
                line_span(b, h.line, h.line_start, h.line)
                    .map(|(s, e)| String::from_utf8_lossy(&b[s..e]).into_owned())
            }) {
                Some(t) => t,
                None => {
                    let mut t = String::from_utf8_lossy(&h.raw).into_owned();
                    t.push('\n');
                    t
                }
            };
            let subs: Vec<serde_json::Value> = h.raw_submatches().iter().map(|&(s, e)| json!({"match":{"text":String::from_utf8_lossy(&h.raw[s as usize..e as usize])},"start":s,"end":e})).collect();
            let sym = h.chain.last().map(|(k, n)| json!({"name": n, "kind": k.name(), "container": chain_str(&h.chain[..h.chain.len()-1])}));
            file_matches += subs.len();
            file_lines += 1;
            printed_bytes += text.len();
            serde_json::to_writer(
                &mut *w,
                &json!({"type":"match","data":{
                    "path":{"text":f.rel},
                    "lines":{"text":text},
                    "line_number":h.line,
                    "absolute_offset":h.line_start,
                    "submatches":subs,
                    "kind":h.kind.name(),
                    "symbol":sym,
                    "file_flags":f.flags.names(),
                    "score":h.score,
                    "clipped":h.clipped
                }}),
            )?;
            writeln!(w)?;
            Ok(())
        };
        for sh in hits {
            let h = &f.hits[sh.hit];
            match (&sh.context, src) {
                (Some((first, lines)), Some(b)) if r.opts.explicit_context() => {
                    for (i, _) in lines.iter().enumerate() {
                        let ln = first + i as u32;
                        if ln <= last {
                            continue;
                        }
                        if let Some(&hi) = hit_lines.get(&ln) {
                            emit_match(w, hi)?;
                        } else if let Some((s, e)) = line_span(b, h.line, h.line_start, ln) {
                            let text = String::from_utf8_lossy(&b[s..e]);
                            serde_json::to_writer(
                                &mut *w,
                                &json!({"type":"context","data":{"path":{"text":f.rel},"lines":{"text":text},"line_number":ln,"absolute_offset":s,"submatches":[]}}),
                            )?;
                            writeln!(w)?;
                        }
                        last = ln;
                    }
                }
                _ => {
                    if h.line > last {
                        emit_match(w, sh.hit)?;
                        last = h.line;
                    }
                }
            }
        }
        printed_lines += file_lines;
        printed_matches += file_matches;
        serde_json::to_writer(
            &mut *w,
            &json!({"type":"end","data":{"path":{"text":f.rel},"binary_offset":null,"stats":{"elapsed":{"secs":0,"nanos":0,"human":"0s"},"searches":1,"searches_with_match":1,"bytes_searched":f.size,"bytes_printed":0,"matched_lines":f.total,"matches":file_matches,"shown":file_lines}}}),
        )?;
        writeln!(w)?;
        Ok(())
    };
    if let Some(fc) = &rep.facets {
        serde_json::to_writer(
            &mut *w,
            &json!({"type":"facets","data":{
                "total":r.stats.total_hits,"files":r.stats.files_matched,
                "by_kind":fc.by_kind.iter().map(|(k,n)| json!([k.name(), n])).collect::<Vec<_>>(),
                "by_dir":fc.by_dir,"by_lang":fc.by_lang,"by_flag":fc.by_flag,
                "definitions_total":fc.defs_total,"demoted_definitions":fc.demoted_defs,
                "imported_by":fc.import_files.iter().map(|&fi| files[fi].rel.clone()).collect::<Vec<_>>()
            }}),
        )?;
        writeln!(w)?;
    }
    for sf in &rep.files {
        emit_file(w, sf)?;
    }
    let ft = &rep.footer;
    let el = t0.elapsed() + std::time::Duration::from_secs_f64(r.stats.elapsed_ms / 1e3);
    let elapsed = json!({"secs":el.as_secs(),"nanos":el.subsec_nanos(),"human":format!("{:.6}s", el.as_secs_f64())});
    serde_json::to_writer(
        &mut *w,
        &json!({"type":"summary","data":{"elapsed_total":elapsed,"stats":{"elapsed":elapsed,"searches":r.stats.files_searched,"searches_with_match":r.stats.files_matched,"bytes_searched":r.files.iter().map(|f| f.size).sum::<u64>(),"bytes_printed":printed_bytes,"matched_lines":r.stats.total_hits,"matches":printed_matches,"matched_lines_shown":printed_lines}}}),
    )?;
    writeln!(w)?;
    serde_json::to_writer(
        &mut *w,
        &json!({"type":"footer","data":{
            "hits_shown":ft.hits_shown,"hits_total":ft.hits_total,"files_shown":ft.files_shown,"files_total":ft.files_total,
            "demoted_files":ft.demoted_files,"demoted_hits":ft.demoted_hits,"skipped_binary":ft.skipped_binary,"skipped_huge":ft.skipped_huge,
            "rung":ft.rung.name(),"rung_names":match &ft.rung { greeg_query::Rung::SplitTokens(v) | greeg_query::Rung::Fuzzy(v) => v.clone(), _ => vec![] },"ignored_only":ft.ignored_only,"est_tokens":ft.est_tokens,"elapsed_ms":ft.elapsed_ms,"hints":ft.hints,
            "related":rep.related.iter().map(|(n,c)| json!([n, c])).collect::<Vec<_>>(),
            "layout":format!("{:?}", rep.layout).to_lowercase(),
            "outcome":verbs_out::outcome_json(&Outcome::of_search(r, ft.hits_shown))
        }}),
    )?;
    writeln!(w)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_join_like_ripgrep() {
        assert_eq!(join_patterns(&["a".into()], true), ("a".into(), true));
        assert_eq!(
            join_patterns(&["a".into(), "b(".into()], false),
            ("(?:a)|(?:b()".into(), false)
        );
        assert_eq!(
            join_patterns(&["a.b".into(), "-x".into()], true),
            (r"(?:a\.b)|(?:\-x)".into(), false)
        );
    }

    #[test]
    fn locations_parse_with_and_without_a_column() {
        assert_eq!(
            parse_location("src/a.rs:12").unwrap(),
            ("src/a.rs".into(), 12)
        );
        assert_eq!(
            parse_location("src/a.rs:12:5").unwrap(),
            ("src/a.rs".into(), 12)
        );
        assert_eq!(
            parse_location("c:/x/a.rs:7").unwrap(),
            ("c:/x/a.rs".into(), 7)
        );
        assert!(parse_location("src/a.rs").is_err());
        assert!(parse_location("src/a.rs:x").is_err());
        assert!(parse_location("12").is_err());
    }

    #[test]
    fn containers_collapse_to_the_last_two() {
        use greeg_lang::DefKind::*;
        let chain = vec![
            (Function, "outer".to_string()),
            (Class, "Cls".to_string()),
            (Method, "m".to_string()),
        ];
        assert_eq!(container_of(&chain, false, false), "Cls.m");
        assert_eq!(container_of(&chain, true, false), "outer.Cls");
        assert_eq!(container_of(&chain, false, true), "outer › Cls › m");
        assert_eq!(container_of(&chain[..1], true, false), "");
    }

    #[test]
    fn line_spans_walk_both_ways() {
        let b = b"aa\nbb\r\ncc\ndd";
        assert_eq!(line_span(b, 3, 7, 3), Some((7, 10)));
        assert_eq!(line_span(b, 3, 7, 1), Some((0, 3)));
        assert_eq!(line_span(b, 3, 7, 2), Some((3, 7)));
        assert_eq!(line_span(b, 3, 7, 4), Some((10, 12)));
        assert_eq!(line_span(b, 3, 7, 5), None);
    }

    #[test]
    fn cli_accepts_ripgrep_cosmetic_flags() {
        for args in [
            vec![
                "greeg",
                "-N",
                "-H",
                "--color",
                "never",
                "--no-heading",
                "-p",
                "--column",
                "foo",
            ],
            vec!["greeg", "-uu", "--no-config", "--trim", "-h", "foo"],
            vec!["greeg", "-e", "-x", "-e", "y", "src"],
            vec!["greeg", "--sort", "path", "-l", "foo"],
        ] {
            let cli = Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert!(cli.cmd.is_none());
        }
        let cli = Cli::try_parse_from(["greeg", "-e", "-x", "-e", "y", "src"]).unwrap();
        assert_eq!(cli.regexp, vec!["-x", "y"]);
        assert_eq!(cli.pattern.as_deref(), Some("src"));
        let cli = Cli::try_parse_from(["greeg", "-uu", "foo"]).unwrap();
        assert_eq!(cli.common.unrestricted, 2);
        assert!(
            Cli::try_parse_from(["greeg", "--help"]).is_err(),
            "--help exits through clap's help action"
        );
    }

    /// A flag greeg cannot honour must fail loudly. `-a` and `-uuu` ask for
    /// binary files, which the index never holds and scan mode stops at; an
    /// answer under them would be missing matches ripgrep reports.
    #[test]
    fn cli_rejects_flags_it_cannot_honour() {
        for (args, want) in [
            (vec!["greeg", "-a", "foo"], "-a/--text"),
            (vec!["greeg", "--text", "foo"], "-a/--text"),
            (vec!["greeg", "-uuu", "foo"], "-uuu"),
        ] {
            let cli = Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            let err = build_options(&cli.common, "foo".into(), vec![])
                .expect_err(&format!("{args:?} must be rejected"));
            let msg = err.to_string();
            assert!(msg.contains(want), "{args:?}: {msg}");
        }
        // -u and -uu stay supported: they widen the file set, not the bytes
        for args in [vec!["greeg", "-u", "foo"], vec!["greeg", "-uu", "foo"]] {
            let cli = Cli::try_parse_from(&args).unwrap();
            let o = build_options(&cli.common, "foo".into(), vec![]).unwrap();
            assert!(o.no_ignore, "{args:?}");
        }
    }
}
