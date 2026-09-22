//! Scan-mode search: walk, match with ripgrep's crates, classify each hit with
//! the language layer, score it, and hand a `ScanResult` to `shape`.

pub mod indexed;
pub mod precise;
pub mod session;
pub mod shape;
pub mod stdin;
pub mod tokens;
pub mod verbs;

use anyhow::{Context, Result};
use greeg_lang::defs::{Outline, outline};
use greeg_lang::lexer::{Lexed, SpanKind, lex};
use greeg_lang::{DefKind, FileFlags, Lang, content_flags, is_import_line, path_flags};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant, SystemTime};

pub const MAX_HITS_PER_FILE: usize = 64;
/// Definition lines kept per file beyond `MAX_HITS_PER_FILE` (C10).
pub const MAX_DEFS_PER_FILE: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Files,
    Count,
    Outline,
    Content,
    Block,
}

/// Whether an empty answer may retry with relaxed word/case/name matching.
/// Exact still honors the caller's explicit flags, including `-i` and `-S`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchingPolicy {
    Exact,
    Discover,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub pattern: String,
    pub fixed_strings: bool,
    pub case_insensitive: bool,
    pub smart_case: bool,
    pub word: bool,
    pub line_regexp: bool,
    pub multiline: bool,
    pub root: PathBuf,
    pub paths: Vec<PathBuf>,
    pub globs: Vec<String>,
    pub types: Vec<String>,
    pub types_not: Vec<String>,
    pub no_ignore: bool,
    pub hidden: bool,
    pub threads: usize,
    pub max_filesize: u64,
    pub mode: Mode,
    pub budget: usize,
    pub near: Vec<String>,
    pub no_tests: bool,
    pub no_vendored: bool,
    pub no_generated: bool,
    pub all: bool,
    pub kinds: Vec<HitKind>,
    pub matching: MatchingPolicy,
    pub max_columns: usize,
    pub per_file_cap: usize,
    pub context: Option<usize>,
    pub before: usize,
    pub after: usize,
    /// Use the persistent index when one exists (built in the background otherwise).
    pub use_index: bool,
    pub fresh: greeg_index::fresh::Mode,
    pub index_dir: Option<PathBuf>,
    /// Re-parse shown files with tree-sitter for exact call/type/member kinds.
    pub precise: bool,
    /// `--sort path`: path order for `-l`/`-c` (ranked layouts keep score order).
    pub sort_path: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            pattern: String::new(),
            fixed_strings: false,
            case_insensitive: false,
            smart_case: false,
            word: false,
            line_regexp: false,
            multiline: false,
            root: PathBuf::from("."),
            paths: vec![],
            globs: vec![],
            types: vec![],
            types_not: vec![],
            no_ignore: false,
            hidden: false,
            threads: 0,
            max_filesize: 4 << 20,
            mode: Mode::Content,
            budget: 2000,
            near: vec![],
            no_tests: false,
            no_vendored: false,
            no_generated: false,
            all: false,
            kinds: vec![],
            matching: MatchingPolicy::Exact,
            max_columns: 200,
            per_file_cap: 4,
            context: None,
            before: 0,
            after: 0,
            use_index: true,
            fresh: greeg_index::fresh::Mode::Auto,
            index_dir: None,
            precise: false,
            sort_path: false,
        }
    }
}

impl Options {
    /// The user asked for explicit context (`-A/-B/-C`).
    pub fn explicit_context(&self) -> bool {
        self.context.is_some() || self.before > 0 || self.after > 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HitKind {
    Def,
    Import,
    Call,
    Type,
    Member,
    Ident,
    Docstring,
    Comment,
    Str,
}

impl HitKind {
    pub const ALL: [HitKind; 9] = [
        HitKind::Def,
        HitKind::Import,
        HitKind::Call,
        HitKind::Type,
        HitKind::Member,
        HitKind::Ident,
        HitKind::Docstring,
        HitKind::Comment,
        HitKind::Str,
    ];
    pub fn name(self) -> &'static str {
        match self {
            HitKind::Def => "def",
            HitKind::Import => "import",
            HitKind::Call => "call",
            HitKind::Type => "type",
            HitKind::Member => "member",
            HitKind::Ident => "ident",
            HitKind::Docstring => "doc",
            HitKind::Comment => "comment",
            HitKind::Str => "string",
        }
    }
    pub fn parse(s: &str) -> Option<HitKind> {
        Some(match s {
            "def" => HitKind::Def,
            "import" => HitKind::Import,
            "call" => HitKind::Call,
            "type" => HitKind::Type,
            "member" => HitKind::Member,
            "ident" => HitKind::Ident,
            "doc" | "docstring" => HitKind::Docstring,
            "comment" => HitKind::Comment,
            "string" | "str" => HitKind::Str,
            _ => return None,
        })
    }
    /// Ranking weight (calls above imports, imports are the
    /// least informative code kind).
    pub fn weight(self) -> f32 {
        match self {
            HitKind::Def => 1.0,
            HitKind::Call => 0.6,
            HitKind::Type => 0.5,
            HitKind::Member => 0.45,
            HitKind::Import => 0.45,
            HitKind::Ident => 0.4,
            HitKind::Docstring => 0.3,
            HitKind::Comment => 0.25,
            HitKind::Str => 0.25,
        }
    }
    pub fn idx(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Debug)]
pub struct Hit {
    /// First line of the match (1-based).
    pub line: u32,
    /// Absolute byte offset of the (untrimmed) line start.
    pub line_start: u32,
    /// Absolute byte offset of the first submatch on the line.
    pub match_start: u32,
    /// End of the first submatch; may pass the line end under `-U`.
    pub match_end: u32,
    /// Every submatch on this line as absolute byte ranges (first == match_start..match_end).
    pub submatches: Vec<(u32, u32)>,
    pub kind: HitKind,
    /// Enclosing definition chain, outermost first.
    pub chain: Vec<(DefKind, String)>,
    pub def_idx: Option<u32>,
    pub score: f32,
    /// The match is the whole pattern as a word, exact case (`is_exact`).
    pub exact: bool,
    /// Line text without the terminator, trimmed at the start and clipped to
    /// `max_columns` around the match (display form).
    pub text: Vec<u8>,
    /// Match range within `text` (after trimming and clipping).
    pub text_match: (u32, u32),
    pub clipped: bool,
    /// The untrimmed line without its terminator (ripgrep's `lines.text` minus `\n`).
    pub raw: Vec<u8>,
}

impl Hit {
    /// Column (byte offset) of the first submatch within `raw`.
    pub fn column(&self) -> u32 {
        self.match_start - self.line_start
    }
    /// Submatch ranges relative to `raw`, clamped to the line.
    pub fn raw_submatches(&self) -> Vec<(u32, u32)> {
        let len = self.raw.len() as u32;
        self.submatches
            .iter()
            .map(|&(s, e)| {
                (
                    (s - self.line_start).min(len),
                    (e - self.line_start).min(len),
                )
            })
            .collect()
    }
}

/// A file's bytes plus its line-start table, read once per query and shared
/// by refine, context extraction, block extraction and `--precise`.
#[derive(Clone, Debug, Default)]
pub struct Source {
    pub bytes: Vec<u8>,
    /// `starts[i]` is the byte offset of line `i + 1`.
    starts: Vec<u32>,
}

impl Source {
    pub fn new(bytes: Vec<u8>) -> Source {
        let mut starts = Vec::with_capacity(bytes.len() / 32 + 1);
        starts.push(0);
        for i in memchr::memchr_iter(b'\n', &bytes) {
            starts.push(i as u32 + 1);
        }
        Source { bytes, starts }
    }
    pub fn line_count(&self) -> u32 {
        self.starts.len() as u32
    }
    /// 1-based line number of a byte offset.
    pub fn line_of(&self, off: u32) -> u32 {
        self.starts.partition_point(|&s| s <= off) as u32
    }
    /// Line `n` (1-based) without its terminator (`\r\n` stripped too).
    pub fn line(&self, n: u32) -> Option<&[u8]> {
        let i = n.checked_sub(1)? as usize;
        let s = *self.starts.get(i)? as usize;
        let e = self
            .starts
            .get(i + 1)
            .map(|&e| e as usize - 1)
            .unwrap_or(self.bytes.len());
        let l = &self.bytes[s..e.max(s)];
        Some(l.strip_suffix(b"\r").unwrap_or(l))
    }
    /// Lines `from..=to` (1-based, clamped) as owned rows.
    pub fn lines(&self, from: u32, to: u32) -> (u32, Vec<Vec<u8>>) {
        let from = from.max(1);
        let to = to.min(self.line_count());
        let mut out = Vec::new();
        let mut n = from;
        while n <= to {
            if let Some(l) = self.line(n) {
                out.push(l.to_vec());
            }
            n += 1;
        }
        (from, out)
    }
    /// Last line (1-based) of a byte range starting on `start_line`.
    pub fn end_line(&self, start: u32, end: u32) -> u32 {
        let end = (end as usize).min(self.bytes.len());
        let last = end.saturating_sub(1).max(start as usize);
        self.line_of(last as u32).max(1)
    }
}

#[derive(Clone, Debug)]
pub struct FileResult {
    pub rel: String,
    pub path: PathBuf,
    pub lang: Lang,
    pub flags: FileFlags,
    pub size: u64,
    pub age_days: f32,
    /// Modification time in Unix seconds (0 when unknown).
    pub mtime: u64,
    pub prior: f32,
    pub hits: Vec<Hit>,
    /// Matched lines in the file after `--kind` filtering (lines beyond the
    /// cap are counted only; with a kind filter the count is of retained hits).
    pub total: usize,
    /// Matched lines before any `--kind` filter.
    pub total_unfiltered: usize,
    pub kinds: [u32; 9],
    /// Definitions referenced by `hits[..].def_idx` (only those; a file can hold thousands).
    pub defs: Vec<DefSummary>,
    /// Kinds, chains and `defs` are final (from the index); `refine` skips the file.
    pub refined: bool,
    /// Index file id when the result came from the index.
    pub file_id: Option<u32>,
    /// The file's bytes once something needed them (read at most once per query).
    pub src: Option<Source>,
}

impl FileResult {
    pub(crate) fn empty() -> FileResult {
        FileResult {
            rel: String::new(),
            path: PathBuf::new(),
            lang: Lang::None,
            flags: FileFlags::default(),
            size: 0,
            age_days: 0.0,
            mtime: 0,
            prior: 0.0,
            hits: vec![],
            total: 0,
            total_unfiltered: 0,
            kinds: [0; 9],
            defs: vec![],
            refined: false,
            file_id: None,
            src: None,
        }
    }
    /// Read the file (once) and return it.
    pub fn source(&mut self) -> Option<&Source> {
        if self.src.is_none() {
            self.src = Some(Source::new(greeg_lang::read_text(&self.path).ok()?));
        }
        self.src.as_ref()
    }
}

#[derive(Clone, Debug)]
pub struct DefSummary {
    pub name: String,
    pub kind: DefKind,
    pub line: u32,
    pub start: u32,
    pub end: u32,
    pub chain: Vec<(DefKind, String)>,
    /// Symbol flags (SYM_EXPORTED, SYM_HAS_DOC, SYM_TEST) when known.
    pub flags: u8,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub files_walked: usize,
    pub files_searched: usize,
    pub files_matched: usize,
    /// Matched lines after `--kind` filtering.
    pub total_hits: usize,
    /// Matched lines before `--kind` filtering.
    pub total_unfiltered: usize,
    pub by_kind: [usize; 9],
    pub skipped_binary: usize,
    pub skipped_huge: usize,
    pub demoted_files: usize,
    pub demoted_hits: usize,
    pub minified_hits: usize,
    pub elapsed_ms: f64,
    pub threads: usize,
    pub cpu_read_ms: f64,
    pub cpu_search_ms: f64,
    pub cpu_classify_ms: f64,
    pub refine_ms: f64,
    /// "scan", "index" or "index+scan" (index unavailable this query)
    pub source: &'static str,
    pub candidates: usize,
    pub fresh_method: &'static str,
    pub fresh_ms: f64,
    pub fresh_changed: usize,
    /// Changed files answered from disk this query; the delta follows the answer.
    pub fresh_deferred: usize,
    pub plan: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Rung {
    #[default]
    Exact,
    NoWord,
    CaseInsensitive,
    /// Split-token search over the symbol names (rung 3): the names that matched.
    SplitTokens(Vec<String>),
    /// Fuzzy (Levenshtein) search over the symbol names (rung 4).
    Fuzzy(Vec<String>),
    Ignored,
}

impl Rung {
    pub fn name(&self) -> &'static str {
        match self {
            Rung::Exact => "exact",
            Rung::NoWord => "without word boundary",
            Rung::CaseInsensitive => "case-insensitive",
            Rung::SplitTokens(_) => "split tokens",
            Rung::Fuzzy(_) => "fuzzy",
            Rung::Ignored => "in ignored/hidden files",
        }
    }
    /// Human description including the substituted names.
    pub fn describe(&self) -> String {
        match self {
            Rung::SplitTokens(v) | Rung::Fuzzy(v) => format!("{} → {}", self.name(), v.join(", ")),
            _ => self.name().to_string(),
        }
    }
}

#[derive(Debug)]
pub struct ScanResult {
    pub opts: Options,
    pub files: Vec<FileResult>,
    pub stats: Stats,
    pub rung: Rung,
    /// Hits that exist only in gitignored/hidden files (rung 5): (files, hits).
    pub ignored_only: Option<(usize, usize)>,
    /// Rung 5 stopped at its time/count bound: `ignored_only` is a lower bound.
    pub ignored_partial: bool,
    /// Identifiers containing the query, with file counts, from the index's
    /// word dictionary (a bare identifier answered from the word postings has
    /// no near-miss hits of its own).
    pub related_index: Vec<(String, usize)>,
}

pub(crate) fn default_threads() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    if cfg!(target_os = "macos") {
        cores.min(4)
    } else {
        cores
    }
}

pub(crate) fn build_matcher(o: &Options) -> Result<RegexMatcher> {
    let mut b = RegexMatcherBuilder::new();
    b.case_insensitive(o.case_insensitive)
        .case_smart(o.smart_case && !o.case_insensitive)
        .word(o.word)
        .multi_line(true)
        .fixed_strings(o.fixed_strings)
        .whole_line(o.line_regexp);
    if !o.multiline {
        b.line_terminator(Some(b'\n'));
    }
    b.build(&o.pattern)
        .with_context(|| format!("invalid pattern {:?}", o.pattern))
}

/// ripgrep's default file types plus greeg aliases (`rs`, `kt`, `python`, …)
/// and the runtime-loaded languages; `-t`/`-T` are applied with ripgrep's
/// precedence (a whitelist glob wins over a type filter).
pub(crate) fn build_types(o: &Options) -> Result<ignore::types::Types> {
    let mut tb = ignore::types::TypesBuilder::new();
    tb.add_defaults();
    for l in greeg_lang::extra::registry() {
        for e in &l.extensions {
            let _ = tb.add(l.name, &format!("*.{e}"));
        }
    }
    let alias = |t: &str| -> String {
        match t {
            "rs" => "rust",
            "kt" => "kotlin",
            "python" => "py",
            "javascript" => "js",
            "typescript" => "ts",
            "text" => "txt",
            x => x,
        }
        .to_string()
    };
    for t in &o.types {
        tb.select(&alias(t));
    }
    for t in &o.types_not {
        tb.negate(&alias(t));
    }
    tb.build()
        .map_err(|e| anyhow::anyhow!("unrecognized file type: {e}"))
}

/// Bounds for one scan pass (used by ladder rung 5, which must stay cheap).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ScanBounds {
    pub(crate) deadline: Option<Instant>,
    /// Never descend into `.git/`.
    pub(crate) skip_git: bool,
    /// Stop after this many matched files (0 = unbounded).
    pub(crate) max_matched: usize,
}

fn walker(o: &Options, threads: usize, bounds: &ScanBounds) -> Result<ignore::WalkParallel> {
    let mut roots: Vec<PathBuf> = if o.paths.is_empty() {
        vec![o.root.clone()]
    } else {
        o.paths.clone()
    };
    roots.dedup();
    let mut wb = ignore::WalkBuilder::new(&roots[0]);
    for r in &roots[1..] {
        wb.add(r);
    }
    wb.hidden(!o.hidden)
        .git_ignore(!o.no_ignore)
        .git_global(!o.no_ignore)
        .git_exclude(!o.no_ignore)
        .ignore(!o.no_ignore)
        .parents(!o.no_ignore)
        .threads(threads);
    if bounds.skip_git {
        wb.filter_entry(|e| e.file_name() != ".git");
    }
    if !o.types.is_empty() || !o.types_not.is_empty() {
        wb.types(build_types(o)?);
    }
    if !o.globs.is_empty() {
        let mut ob = ignore::overrides::OverrideBuilder::new(&o.root);
        for g in &o.globs {
            ob.add(g).with_context(|| format!("bad glob {g:?}"))?;
        }
        wb.overrides(ob.build()?);
    }
    Ok(wb.build_parallel())
}

/// One matched line as recorded by the sink: line number, absolute line
/// start, and the submatches on that line (absolute ranges).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LineHit {
    pub(crate) line: u32,
    pub(crate) line_start: u32,
    pub(crate) subs: Vec<(u32, u32)>,
}

/// Sink that groups matches by line: the first `cap` matched lines are
/// recorded with all their submatches, later lines are only counted, except
/// definition lines (the match overlaps the defined name), which are kept up
/// to `def_cap` so a definition after 64 uses is never hidden (C10).
pub(crate) struct CollectSink<'a> {
    pub(crate) matcher: &'a RegexMatcher,
    pub(crate) lang: Lang,
    pub(crate) hits: Vec<LineHit>,
    /// Matched lines (ripgrep's `-c` count).
    pub(crate) total: usize,
    pub(crate) max_per_line: usize,
    pub(crate) cap: usize,
    pub(crate) keep_defs: bool,
    pub(crate) def_cap: usize,
    pub(crate) defs_kept: usize,
    /// `-l`: stop after the first matched line.
    pub(crate) first_only: bool,
    /// Offset of the searched slice within the file (3 after a UTF-8 BOM).
    pub(crate) base: u32,
    pub(crate) binary: bool,
    pub(crate) multiline: bool,
}

impl<'a> CollectSink<'a> {
    pub(crate) fn new(
        matcher: &'a RegexMatcher,
        lang: Lang,
        cap: usize,
        keep_defs: bool,
        multiline: bool,
    ) -> CollectSink<'a> {
        CollectSink {
            matcher,
            lang,
            hits: Vec::new(),
            total: 0,
            max_per_line: 8,
            cap,
            keep_defs,
            def_cap: MAX_DEFS_PER_FILE,
            defs_kept: 0,
            first_only: false,
            base: 0,
            binary: false,
            multiline,
        }
    }
}

impl Sink for CollectSink<'_> {
    type Error = io::Error;
    fn matched(&mut self, _s: &Searcher, m: &SinkMatch<'_>) -> Result<bool, io::Error> {
        let block_line = m.line_number().unwrap_or(0) as u32;
        let block_start = m.absolute_byte_offset() as u32 + self.base;
        let bytes = m.bytes();
        // a multi-line block (only under -U): line numbers are counted per match
        let multi = self.multiline
            && memchr::memchr(b'\n', &bytes[..bytes.len().saturating_sub(1)]).is_some();
        let mut nl_pos = 0usize; // scanned up to here for newlines
        let mut nl_count = 0u32;
        let mut line_off = 0usize; // start of the current line within `bytes`
        let mut last_line: Option<u32> = None;
        let mut last_kept = false;
        let mut stop = false;
        let _ = self.matcher.find_iter(bytes, |mat| {
            if multi {
                for i in memchr::memchr_iter(b'\n', &bytes[nl_pos..mat.start()]) {
                    nl_count += 1;
                    line_off = nl_pos + i + 1;
                }
                nl_pos = mat.start();
            }
            let line = block_line + nl_count;
            let ms = block_start + mat.start() as u32;
            let me = block_start + mat.end() as u32;
            if last_line == Some(line) {
                if last_kept
                    && let Some(h) = self.hits.last_mut()
                    && h.subs.len() < self.max_per_line
                {
                    h.subs.push((ms, me));
                }
                return true;
            }
            last_line = Some(line);
            self.total += 1;
            let keep = if self.hits.len() < self.cap {
                true
            } else if self.keep_defs && self.defs_kept < self.def_cap {
                let le = memchr::memchr(b'\n', &bytes[mat.start()..])
                    .map(|k| mat.start() + k)
                    .unwrap_or(bytes.len());
                let l = &bytes[line_off..le];
                let (lms, lme) = (mat.start() - line_off, mat.end().min(le) - line_off);
                let is_def = greeg_lang::defs::def_name_on_line(self.lang, l)
                    .map(|(ns, ne)| ns < lme && lms < ne)
                    .unwrap_or(false);
                if is_def {
                    self.defs_kept += 1;
                }
                is_def
            } else {
                false
            };
            last_kept = keep;
            if keep {
                self.hits.push(LineHit {
                    line,
                    line_start: block_start + line_off as u32,
                    subs: vec![(ms, me)],
                });
            }
            if self.first_only {
                stop = true;
                return false;
            }
            true
        });
        Ok(!stop)
    }
    fn binary_data(&mut self, _s: &Searcher, _off: u64) -> Result<bool, io::Error> {
        self.binary = true;
        Ok(false)
    }
}

fn near_weight(rel: &str, near: &[String]) -> f32 {
    if near.is_empty() {
        return 1.0;
    }
    let dir = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let mut best = 0.7f32;
    for n in near {
        let n = n.trim_end_matches('/');
        let ndir = if Path::new(n).extension().is_some() {
            n.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
        } else {
            n
        };
        if rel.starts_with(n) || dir == ndir || (ndir.is_empty() && !rel.contains('/')) {
            return 1.0;
        }
        let parent = |d: &str| {
            d.rsplit_once('/')
                .map(|(p, _)| p.to_string())
                .unwrap_or_default()
        };
        if parent(dir) == parent(ndir) {
            best = best.max(0.85);
        }
    }
    best
}

const MOCK_SEGMENTS: &[&str] = &[
    "mock",
    "mocks",
    "__mocks__",
    "stub",
    "stubs",
    "fake",
    "fakes",
];

/// A path with a `mock`/`stub`/`fake` directory or file stem: demoted like tests.
pub fn is_mock_path(rel: &str) -> bool {
    let mut parts = rel.split('/').peekable();
    while let Some(seg) = parts.next() {
        let is_file = parts.peek().is_none();
        let stem = if is_file {
            seg.split('.').next().unwrap_or(seg)
        } else {
            seg
        };
        if stem.len() <= 10 && MOCK_SEGMENTS.iter().any(|m| m.eq_ignore_ascii_case(stem)) {
            return true;
        }
    }
    false
}

/// Location weight shared by search and the verbs.
pub(crate) fn loc_weight(flags: FileFlags, rel: &str, all: bool) -> f32 {
    if all {
        1.0
    } else if flags.has(FileFlags::MINIFIED) {
        0.1
    } else if flags.has(FileFlags::GENERATED | FileFlags::VENDORED | FileFlags::LOCKFILE) {
        0.2
    } else if flags.has(FileFlags::TEST) || is_mock_path(rel) {
        0.45
    } else {
        1.0
    }
}

/// File prior: location × near. (Recency from mtime is not used: every file
/// of a fresh clone is "today".)
pub(crate) fn file_prior(flags: FileFlags, rel: &str, o: &Options) -> f32 {
    loc_weight(flags, rel, o.all) * 0.8 * near_weight(rel, &o.near)
}

/// Last whitespace-delimited identifier at the end of `head`, if `head` ends with one.
fn last_word(head: &[u8]) -> &[u8] {
    let mut i = head.len();
    while i > 0 && is_word_byte(head[i - 1]) {
        i -= 1;
    }
    &head[i..]
}

fn trim_end_ws(s: &[u8]) -> &[u8] {
    let mut e = s.len();
    while e > 0 && (s[e - 1] == b' ' || s[e - 1] == b'\t') {
        e -= 1;
    }
    &s[..e]
}

/// `head` ends with the keyword `kw` as a whole word.
fn ends_kw(head: &[u8], kw: &[u8]) -> bool {
    head.ends_with(kw) && (head.len() == kw.len() || !is_word_byte(head[head.len() - kw.len() - 1]))
}

/// Innermost unclosed bracket in `head` (line-local), if any.
fn open_bracket(head: &[u8]) -> Option<u8> {
    let (mut paren, mut brace, mut sq) = (0i32, 0i32, 0i32);
    for &b in head.iter().rev() {
        match b {
            b')' => paren += 1,
            b'}' => brace += 1,
            b']' => sq += 1,
            b'(' if paren > 0 => paren -= 1,
            b'{' if brace > 0 => brace -= 1,
            b'[' if sq > 0 => sq -= 1,
            b'(' | b'{' | b'[' => return Some(b),
            _ => {}
        }
    }
    None
}

/// First keyword of a statement line, skipping closing braces and `else`.
fn stmt_keyword(line: &[u8]) -> &[u8] {
    let mut t = greeg_lang::trim_start(line);
    loop {
        t = greeg_lang::trim_start(t);
        if let Some(rest) = t.strip_prefix(b"}") {
            t = rest;
            continue;
        }
        if t.starts_with(b"else") && !t.get(4).map(|&b| is_word_byte(b)).unwrap_or(false) {
            t = &t[4..];
            continue;
        }
        break;
    }
    let mut i = 0;
    while i < t.len() && is_word_byte(t[i]) {
        i += 1;
    }
    &t[..i]
}

/// Position of the byte after the `>` closing a `<` at `open` (balanced, line-local).
fn close_angle(line: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, &b) in line.iter().enumerate().skip(open) {
        match b {
            b'<' => depth += 1,
            b'>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            b';' | b'{' | b'}' => return None,
            _ => {}
        }
    }
    None
}

/// Byte-context rule for kinds that are not stored as spans,
/// language-aware. `ms`/`me` are offsets into `_src`; `line` is the line
/// containing the match and starts at `line_start` in `_src`. Only the line is
/// consulted, so every rule is line-local.
pub(crate) fn kind_by_context(
    lang: Lang,
    _src: &[u8],
    ms: usize,
    me: usize,
    line_start: usize,
    line: &[u8],
) -> HitKind {
    if is_import_line(lang, line) {
        return HitKind::Import;
    }
    let lms = ms.saturating_sub(line_start).min(line.len());
    let lme = me.saturating_sub(line_start).clamp(lms, line.len());
    let (rust, kotlin, ts, js, py) = (
        lang == Lang::Rust,
        lang == Lang::Kotlin,
        lang == Lang::TypeScript,
        lang == Lang::JavaScript,
        lang == Lang::Python,
    );
    // context after the match
    let after_i = {
        let mut i = lme;
        while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
            i += 1;
        }
        i
    };
    let after = line.get(after_i).copied().unwrap_or(0);
    let after_tight = line.get(lme).copied().unwrap_or(0); // no whitespace skipped
    let after2 = line.get(after_i + 1).copied().unwrap_or(0);
    // context before the match
    let head = trim_end_ws(&line[..lms]);
    let prev_tight = if lms > 0 { line[lms - 1] } else { 0 };
    let prev = head.last().copied().unwrap_or(0);
    let word = last_word(head);
    let kw = stmt_keyword(line);
    let kw_off = kw.as_ptr() as usize - line.as_ptr() as usize;
    let ends_sym = |s: &[u8]| head.ends_with(s);
    let double_colon = ends_sym(b"::");
    let member_or_ident = |prev_tight: u8| {
        if prev_tight == b'.' {
            HitKind::Member
        } else {
            HitKind::Ident
        }
    };
    // Rust forbids struct literals in `if`/`while`/`match`/`for` heads: `X {` there opens the block
    let rust_block_head =
        rust && matches!(kw, b"if" | b"while" | b"match" | b"for") && kw_off < lms;
    let decl_line = match lang {
        Lang::Kotlin => matches!(
            kw,
            b"class"
                | b"interface"
                | b"object"
                | b"enum"
                | b"data"
                | b"sealed"
                | b"abstract"
                | b"open"
                | b"private"
                | b"public"
                | b"internal"
                | b"inner"
                | b"annotation"
                | b"fun"
                | b"override"
                | b"protected"
                | b"companion"
                | b"value"
                | b"typealias"
        ),
        Lang::TypeScript | Lang::JavaScript => matches!(
            kw,
            b"class" | b"export" | b"interface" | b"abstract" | b"declare"
        ),
        Lang::Python => matches!(kw, b"class" | b"except"),
        _ => false,
    };
    let type_kw = match lang {
        Lang::Rust => {
            matches!(
                word,
                b"impl"
                    | b"dyn"
                    | b"as"
                    | b"struct"
                    | b"trait"
                    | b"enum"
                    | b"type"
                    | b"union"
                    | b"where"
            ) || (word == b"for" && kw == b"impl")
        }
        Lang::Kotlin => matches!(
            word,
            b"is" | b"as" | b"class" | b"interface" | b"object" | b"typealias"
        ),
        Lang::TypeScript => matches!(
            word,
            b"extends"
                | b"implements"
                | b"as"
                | b"is"
                | b"instanceof"
                | b"typeof"
                | b"keyof"
                | b"interface"
                | b"class"
                | b"satisfies"
        ),
        Lang::JavaScript => matches!(word, b"extends" | b"instanceof" | b"class"),
        Lang::Python => matches!(word, b"except" | b"class"),
        _ => matches!(
            word,
            b"extends" | b"implements" | b"instanceof" | b"struct" | b"class" | b"interface"
        ),
    };
    let value_kw = !word.is_empty()
        && match lang {
            Lang::Rust => {
                matches!(
                    word,
                    b"let"
                        | b"match"
                        | b"in"
                        | b"if"
                        | b"while"
                        | b"return"
                        | b"ref"
                        | b"move"
                        | b"break"
                        | b"continue"
                        | b"await"
                ) || (word == b"for" && kw != b"impl")
            }
            Lang::Kotlin => matches!(
                word,
                b"val"
                    | b"var"
                    | b"in"
                    | b"when"
                    | b"if"
                    | b"while"
                    | b"return"
                    | b"throw"
                    | b"by"
                    | b"do"
                    | b"else"
            ),
            Lang::TypeScript | Lang::JavaScript => {
                matches!(
                    word,
                    b"const"
                        | b"let"
                        | b"var"
                        | b"return"
                        | b"await"
                        | b"yield"
                        | b"in"
                        | b"of"
                        | b"case"
                        | b"throw"
                        | b"delete"
                        | b"void"
                        | b"if"
                        | b"while"
                        | b"else"
                        | b"do"
                ) || (js && word == b"typeof")
            }
            Lang::Python => matches!(
                word,
                b"in"
                    | b"not"
                    | b"and"
                    | b"or"
                    | b"if"
                    | b"while"
                    | b"return"
                    | b"yield"
                    | b"await"
                    | b"del"
                    | b"assert"
                    | b"elif"
                    | b"for"
                    | b"lambda"
                    | b"global"
                    | b"nonlocal"
                    | b"is"
                    | b"as"
                    | b"else"
                    | b"with"
                    | b"print"
            ),
            _ => false,
        };
    // 1. calls: `foo(`, `new Foo`, `foo<T>(`, trailing lambda / struct literal in expression position
    if after == b'(' || (lme > lms && line[lme - 1] == b'(') {
        // `foo(`; or the match itself ends with the paren (`foo\(` patterns)
        return HitKind::Call;
    }
    if !rust && !py && !kotlin && (ends_kw(head, b"new") || ends_kw(head, b"await new")) {
        return HitKind::Call;
    }
    let generic_lang = rust || ts || kotlin;
    if generic_lang && after_tight == b'<' && !matches!(after2, b' ' | b'\t' | b'0'..=b'9' | b'=') {
        // `foo<T>(x)` is a call with type arguments; `Foo<T>` alone is a type
        let is_call = close_angle(line, lme)
            .map(|c| greeg_lang::trim_start(&line[c..]).first() == Some(&b'('))
            .unwrap_or(false);
        return if is_call {
            HitKind::Call
        } else {
            HitKind::Type
        };
    }
    let value_pos = value_kw && !ends_kw(head, b"return");
    let literal_lang = (rust && !rust_block_head) || (kotlin && !decl_line);
    if after == b'{' && literal_lang && !type_kw && !value_pos {
        let expr_pos = head.is_empty()
            || double_colon
            || (prev == b'='
                && !(ends_sym(b"==")
                    || ends_sym(b"!=")
                    || ends_sym(b"<=")
                    || ends_sym(b">=")
                    || ends_sym(b"=>")))
            || matches!(prev, b'(' | b',' | b'.' | b'[' | b'!' | b'|')
            || ends_kw(head, b"return");
        if expr_pos && !(head.is_empty() && block_keyword_line(line)) {
            return HitKind::Call;
        }
    }
    // 2. value-position keywords: `let mut X`, `for x in X`, `match X`, `return X`
    if value_kw {
        return member_or_ident(prev_tight);
    }
    // 3. type contexts
    if type_kw {
        return HitKind::Type;
    }
    // supertype lists: Kotlin `class A : B, C {`, TS `implements B, C {`, Python `class A(B, C):`
    if prev == b',' && decl_line {
        let in_list = if py {
            open_bracket(head) == Some(b'(')
        } else {
            memchr::memmem::find(head, b" : ").is_some()
                || memchr::memmem::find(head, b" implements ").is_some()
                || memchr::memmem::find(head, b" extends ").is_some()
        };
        if in_list {
            return HitKind::Type;
        }
    }
    if py && prev == b'(' && decl_line {
        return HitKind::Type;
    }
    // Rust reference / pointer / lifetime prefixes: strip them and look further back
    if rust {
        let mut h = head;
        let mut stripped = false;
        loop {
            let t = trim_end_ws(h);
            if ends_kw(t, b"mut") || ends_kw(t, b"dyn") {
                h = &t[..t.len() - 3];
                stripped = true;
                continue;
            }
            if ends_kw(t, b"const") {
                h = &t[..t.len() - 5];
                stripped = true;
                continue;
            }
            let w = last_word(t);
            if !w.is_empty() && t.len() > w.len() && t[t.len() - w.len() - 1] == b'\'' {
                h = &t[..t.len() - w.len() - 1];
                stripped = true;
                continue;
            }
            if t.ends_with(b"&") || t.ends_with(b"*") {
                // `a & b` / `a * b` (spaced on both sides) is a binary operator, not a prefix
                let spaced = t.len() >= 2
                    && matches!(t[t.len() - 2], b' ' | b'\t')
                    && lms > 0
                    && matches!(line[lms - 1], b' ' | b'\t');
                if spaced && !stripped {
                    return member_or_ident(prev_tight);
                }
                h = &t[..t.len() - 1];
                stripped = true;
                continue;
            }
            break;
        }
        if stripped {
            let t = trim_end_ws(h);
            let w = last_word(t);
            let generic = t.ends_with(b"<")
                || (t.ends_with(b",")
                    && open_bracket(t).is_none()
                    && memchr::memchr(b'<', t).is_some());
            let type_ctx = (t.ends_with(b":") && !t.ends_with(b"::"))
                || t.ends_with(b"->")
                || generic
                || (kw == b"fn" && (t.ends_with(b"(") || t.ends_with(b",")))
                || matches!(w, b"impl" | b"dyn" | b"as" | b"for" | b"where");
            return if type_ctx {
                HitKind::Type
            } else {
                HitKind::Ident
            };
        }
        // `Semaphore::new`: an uppercase path qualifier is a type
        if after_tight == b':' && after2 == b':' && line[lms].is_ascii_uppercase() {
            return HitKind::Type;
        }
    }
    // `a: T` — annotation, or the value of a `key: value` pair in object/dict literals
    if prev == b':' && !double_colon && !ends_sym(b":=") {
        let is_type = if rust || kotlin {
            true
        } else {
            let key_head = trim_end_ws(&head[..head.len() - 1]);
            let key_is_string = key_head.ends_with(b"\"") || key_head.ends_with(b"'");
            match open_bracket(key_head) {
                Some(b'(') => !(js || key_is_string || (py && kw == b"lambda")),
                Some(_) => false,
                None => {
                    if js || key_is_string {
                        false
                    } else if py {
                        !matches!(
                            kw,
                            b"if"
                                | b"elif"
                                | b"else"
                                | b"for"
                                | b"while"
                                | b"try"
                                | b"except"
                                | b"finally"
                                | b"with"
                                | b"lambda"
                                | b"case"
                                | b"match"
                                | b"def"
                                | b"class"
                        )
                    } else {
                        // TS: `x: Foo;` (member/parameter annotation) vs `x: foo,` (object literal row)
                        let literal_row =
                            after == b',' && !line.contains(&b';') && !line.contains(&b'=');
                        !(matches!(kw, b"case" | b"default") || literal_row)
                    }
                }
            }
        };
        return if is_type {
            HitKind::Type
        } else {
            member_or_ident(prev_tight)
        };
    }
    if ends_sym(b"->") {
        return HitKind::Type;
    }
    // generics: `Foo<Bar>`, `Map<K, Bar>`, `Vec<Foo`; not `a<b` / `i < n`
    if generic_lang
        && prev_tight == b'<'
        && !matches!(after_tight, b' ' | b'\t' | b'0'..=b'9')
        && matches!(
            after,
            b'>' | b',' | b'<' | b'(' | b'[' | b'&' | b'\'' | 0 | b';'
        )
    {
        // `<` must hang off an identifier or `::`: `Vec<`, `::<`
        if head[..head.len() - 1]
            .last()
            .map(|&b| is_word_byte(b) || b == b':' || b == b'>')
            .unwrap_or(false)
        {
            return HitKind::Type;
        }
    }
    if generic_lang
        && (after == b'>' || after == b',')
        && let Some(lt) = memchr::memrchr(b'<', head)
        && lt > 0
        && (is_word_byte(head[lt - 1]) || head[lt - 1] == b':')
        && memchr::memrchr(b'(', head)
            .map(|lp| lt > lp)
            .unwrap_or(true)
        && !head[lt + 1..].contains(&b'>')
    {
        return HitKind::Type;
    }
    // Python subscripted generics: `List[Foo]`, `dict[str, Foo]`
    if py && (prev_tight == b'[' || (prev == b',' && open_bracket(head) == Some(b'['))) {
        let h = if prev_tight == b'[' {
            &head[..head.len() - 1]
        } else {
            &head[..memchr::memrchr(b'[', head).unwrap_or(0)]
        };
        let owner = last_word(trim_end_ws(h));
        if !owner.is_empty()
            && (owner[0].is_ascii_uppercase()
                || matches!(
                    owner,
                    b"list" | b"dict" | b"set" | b"tuple" | b"type" | b"frozenset"
                ))
        {
            return HitKind::Type;
        }
    }
    // property key: `{ Foo: 1 }`, `Foo: string;`, `readonly Foo: T`
    if !py && after == b':' && after2 != b':' && after2 != b'=' {
        let modifier = matches!(
            word,
            b"readonly"
                | b"private"
                | b"public"
                | b"protected"
                | b"static"
                | b"declare"
                | b"abstract"
                | b"override"
                | b"pub"
        );
        if matches!(prev, b'{' | b',')
            || modifier
            || (head.is_empty() && !matches!(kw, b"case" | b"default"))
        {
            return HitKind::Member;
        }
    }
    if prev_tight == b'.' || double_colon || (ts && ends_sym(b"?.")) {
        return HitKind::Member;
    }
    if generic_lang && prev_tight == b'<' && after_tight == b'>' {
        return HitKind::Type;
    }
    HitKind::Ident
}

/// A line whose first token is a block keyword with a trailing `{` (`else {`,
/// `unsafe {`): never a call.
fn block_keyword_line(line: &[u8]) -> bool {
    let t = greeg_lang::trim_start(line);
    let mut i = 0;
    while i < t.len() && is_word_byte(t[i]) {
        i += 1;
    }
    matches!(
        &t[..i],
        b"else" | b"try" | b"finally" | b"do" | b"loop" | b"unsafe" | b"async" | b"move" | b"init"
    )
}

/// Cheap, line-local classification used during the scan (no file outline).
fn classify_line(lang: Lang, line: &[u8], ms: usize, me: usize) -> HitKind {
    if !lang.has_grammar() {
        return HitKind::Ident;
    }
    // a line without quote or comment starters has no noncode spans: skip the lexer
    if memchr::memchr3(b'"', b'\'', b'/', line).is_some()
        || memchr::memchr2(b'#', b'`', line).is_some()
    {
        let lexed = lex(lang, line);
        if let Some(sp) = lexed.span_at(ms as u32) {
            return match sp.kind {
                SpanKind::Comment => HitKind::Comment,
                SpanKind::Docstring => HitKind::Docstring,
                SpanKind::String => HitKind::Str,
            };
        }
    }
    if let Some((ns, ne)) = greeg_lang::defs::def_name_on_line(lang, line)
        && ns < me
        && ms < ne
    {
        return HitKind::Def;
    }
    kind_by_context(lang, line, ms, me, 0, line)
}

/// Precise classification with a file outline (used for shown files).
#[allow(clippy::too_many_arguments)]
fn classify_full(
    lang: Lang,
    src: &[u8],
    lexed: &Lexed,
    ol: &Outline,
    ms: u32,
    me: u32,
    line_start: u32,
    line: &[u8],
) -> (HitKind, Option<u32>) {
    if let Some(sp) = lexed.span_at(ms) {
        return (
            match sp.kind {
                SpanKind::Comment => HitKind::Comment,
                SpanKind::Docstring => HitKind::Docstring,
                SpanKind::String => HitKind::Str,
            },
            ol.enclosing(ms),
        );
    }
    if let Some(d) = ol.def_named_in(ms, me) {
        return (HitKind::Def, Some(d));
    }
    (
        kind_by_context(
            lang,
            src,
            ms as usize,
            me as usize,
            line_start as usize,
            line,
        ),
        ol.enclosing(ms),
    )
}

pub(crate) fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Score multiplier for a match equal to the query as a whole word: exact
/// definitions (the defined name *is* the query) rank above near-misses like
/// `spawn_blocking_on` (1.3), other exact matches 1.15.
pub(crate) fn exact_boost(kind: HitKind, exact: bool) -> f32 {
    if !exact {
        1.0
    } else if kind == HitKind::Def {
        1.3
    } else {
        1.15
    }
}

/// The match equals the pattern as a whole word, exact case.
pub(crate) fn is_exact(o: &Options, src: &[u8], ms: u32, me: u32) -> bool {
    !o.fixed_strings
        && !o.case_insensitive
        && src.get(ms as usize..me as usize) == Some(o.pattern.as_bytes())
        && (ms == 0 || !is_word_byte(src[ms as usize - 1]))
        && (me as usize >= src.len() || !is_word_byte(src[me as usize]))
}

pub(crate) struct Ctx<'a> {
    pub(crate) o: &'a Options,
    pub(crate) matcher: &'a RegexMatcher,
    pub(crate) stats: &'a StatsAcc,
    pub(crate) classify: bool,
    /// Apply `--kind` inside `process_file` (false when the index classifies afterwards).
    pub(crate) filter_kinds: bool,
}

pub(crate) fn process_file(
    cx: &Ctx,
    path: &Path,
    rel: String,
    searcher: &mut Searcher,
    buf: &mut Vec<u8>,
) -> Option<FileResult> {
    let o = cx.o;
    cx.stats.searched.fetch_add(1, Relaxed);
    // Path-derived exclusions are cheap enough to apply before reading.
    let mut flags = FileFlags::default();
    let need_path_flags = !o.all && (o.no_tests || o.no_vendored || o.no_generated);
    if need_path_flags {
        flags = path_flags(&rel);
        if o.no_tests && flags.has(FileFlags::TEST)
            || o.no_vendored && flags.has(FileFlags::VENDORED)
            || o.no_generated
                && flags.has(FileFlags::GENERATED | FileFlags::MINIFIED | FileFlags::LOCKFILE)
        {
            return None;
        }
    }
    buf.clear();
    let t_read = Instant::now();
    {
        use std::io::Read;
        let f = fs::File::open(path).ok()?;
        // read at most max_filesize + 1 so oversized files are detected without a stat
        f.take(o.max_filesize + 1).read_to_end(buf).ok()?;
    }
    cx.stats
        .read_ns
        .fetch_add(t_read.elapsed().as_nanos() as u64, Relaxed);
    if buf.len() as u64 > o.max_filesize {
        cx.stats.huge.fetch_add(1, Relaxed);
        return None;
    }
    greeg_lang::transcode_utf16(buf);
    let src: &[u8] = buf;
    // A UTF-8 BOM is not part of the first line: search past it (offsets keep
    // indexing the whole buffer, which is what the index's span tables use).
    let bom = if src.starts_with(&[0xEF, 0xBB, 0xBF]) {
        3
    } else {
        0
    };
    let body = &src[bom..];
    if memchr::memchr(0, &body[..body.len().min(8192)]).is_some() {
        cx.stats.binary.fetch_add(1, Relaxed);
        return None;
    }
    let lang = Lang::from_path(path);
    let cap = if o.budget == 0 {
        usize::MAX
    } else {
        MAX_HITS_PER_FILE
    };
    let mut sink = CollectSink::new(
        cx.matcher,
        lang,
        cap,
        cx.classify && lang.has_grammar(),
        o.multiline,
    );
    sink.base = bom as u32;
    sink.first_only = o.mode == Mode::Files && o.kinds.is_empty();
    let t_search = Instant::now();
    let r = searcher.search_slice(cx.matcher, body, &mut sink);
    cx.stats
        .search_ns
        .fetch_add(t_search.elapsed().as_nanos() as u64, Relaxed);
    if sink.binary {
        // a NUL past the first 8 KiB: ripgrep skips the file too; count it
        cx.stats.binary.fetch_add(1, Relaxed);
        return None;
    }
    if r.is_err() || sink.total == 0 {
        return None;
    }
    let t_cls = Instant::now();
    // Only matched files pay for flags, metadata and classification.
    if !need_path_flags {
        flags = path_flags(&rel);
    }
    flags.0 |= content_flags(&body[..body.len().min(65536)], src.len() as u64).0;
    if flags.has(FileFlags::BINARY) {
        cx.stats.binary.fetch_add(1, Relaxed);
        return None;
    }
    let md = fs::metadata(path).ok();
    let size = md.as_ref().map(|m| m.len()).unwrap_or(src.len() as u64);
    let modified = md.and_then(|m| m.modified().ok());
    let mtime = modified
        .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let age_days = modified
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .map(|d| d.as_secs_f32() / 86400.0)
        .unwrap_or(365.0);
    let prior = file_prior(flags, &rel, o);
    let mut hits = Vec::with_capacity(sink.hits.len());
    let mut kinds = [0u32; 9];
    let filtering = cx.filter_kinds && !o.kinds.is_empty();
    for lh in sink.hits {
        let ls = lh.line_start;
        let (ms, me) = lh.subs[0];
        let le = memchr::memchr(b'\n', &src[ls as usize..])
            .map(|k| ls as usize + k)
            .unwrap_or(src.len());
        if ms < ls || (ms as usize) > le {
            if std::env::var_os("GREEG_DEBUG").is_some() {
                eprintln!(
                    "greeg: offset mismatch in {} line {} ls={} le={} ms={} me={}",
                    rel, lh.line, ls, le, ms, me
                );
            }
            continue;
        }
        let line_bytes = &src[ls as usize..le];
        // a multi-line match (-U) is classified and displayed by its first line
        let me_line = (me as usize).min(le) as u32;
        let kind = if cx.classify && !flags.has(FileFlags::MINIFIED) {
            classify_line(
                lang,
                line_bytes,
                (ms - ls) as usize,
                (me_line - ls) as usize,
            )
        } else {
            HitKind::Ident
        };
        if filtering && !o.kinds.contains(&kind) {
            continue;
        }
        kinds[kind.idx()] += 1;
        let exact = is_exact(o, src, ms, me);
        let score = kind.weight() * prior * exact_boost(kind, exact);
        let lead = line_bytes.len()
            - greeg_lang::trim_start(line_bytes)
                .len()
                .min(line_bytes.len());
        let (text, clipped, tm) = clip_line(
            &line_bytes[lead..],
            ((ms - ls) as usize).saturating_sub(lead),
            ((me_line - ls) as usize).saturating_sub(lead),
            o.max_columns,
        );
        let raw = line_bytes
            .strip_suffix(b"\r")
            .unwrap_or(line_bytes)
            .to_vec();
        hits.push(Hit {
            line: lh.line,
            line_start: ls,
            match_start: ms,
            match_end: me,
            submatches: lh.subs,
            kind,
            chain: Vec::new(),
            def_idx: None,
            score,
            exact,
            text,
            text_match: (tm.0 as u32, tm.1 as u32),
            clipped,
            raw,
        });
    }
    cx.stats
        .classify_ns
        .fetch_add(t_cls.elapsed().as_nanos() as u64, Relaxed);
    if hits.is_empty() {
        return None;
    }
    let total = if filtering { hits.len() } else { sink.total };
    Some(FileResult {
        rel,
        path: path.to_path_buf(),
        lang,
        flags,
        size,
        age_days,
        mtime,
        prior,
        hits,
        total,
        total_unfiltered: sink.total,
        kinds,
        defs: Vec::new(),
        refined: false,
        file_id: None,
        src: None,
    })
}

/// Clip a line around the match. Returns the text, whether it was clipped,
/// and the match range within the returned text.
pub fn clip_line(
    line: &[u8],
    ms: usize,
    me: usize,
    max_cols: usize,
) -> (Vec<u8>, bool, (usize, usize)) {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let ms = ms.min(line.len());
    let me = me.clamp(ms, line.len());
    if max_cols == 0 || line.len() <= max_cols {
        return (line.to_vec(), false, (ms, me));
    }
    let window = max_cols.saturating_sub(4);
    let mlen = (me - ms).min(window);
    let lead = (window - mlen) / 2;
    let start = ms.saturating_sub(lead);
    let end = (start + window).min(line.len());
    let start = end.saturating_sub(window);
    let mut v = Vec::with_capacity(max_cols + 4);
    let mut shift = 0usize;
    if start > 0 {
        v.extend_from_slice("…".as_bytes());
        shift = "…".len();
    }
    v.extend_from_slice(&line[start..end]);
    if end < line.len() {
        v.extend_from_slice("…".as_bytes());
    }
    let tms = ms.saturating_sub(start) + shift;
    let tme = (me.min(end)).saturating_sub(start) + shift;
    (v, true, (tms, tme.max(tms)))
}

/// Full outline for one file: refine hit kinds, attach enclosing chains, fill
/// `defs` with the definitions the hits point at. Reads the file once and
/// keeps it in `f.src` for context extraction.
pub fn refine_file(f: &mut FileResult) {
    if f.refined || !f.lang.has_grammar() || f.flags.has(FileFlags::MINIFIED) {
        return;
    }
    let t_read = Instant::now();
    if f.source().is_none() {
        return;
    }
    let read_us = t_read.elapsed().as_micros();
    let Some(source) = f.src.as_ref() else { return };
    let full: &[u8] = &source.bytes;
    // Lex and outline a window around the hits, not the whole file: a hit
    // deep inside a 2.5 MB file must not cost 10 ms of parsing. The window
    // starts at a line boundary up to 512 KiB before the first hit (lexer
    // state is assumed "code" there; a block comment spanning that point is
    // the accepted inaccuracy) and ends 64 KiB after the last hit.
    const BEFORE: usize = 512 * 1024;
    const AFTER: usize = 64 * 1024;
    let first = f
        .hits
        .iter()
        .map(|h| h.line_start as usize)
        .min()
        .unwrap_or(0);
    let last = f
        .hits
        .iter()
        .map(|h| h.match_end as usize)
        .max()
        .unwrap_or(0)
        .min(full.len());
    let start = if first > BEFORE {
        let s = first - BEFORE;
        memchr::memchr(b'\n', &full[s..])
            .map(|k| s + k + 1)
            .unwrap_or(s)
    } else {
        0
    };
    let cut = (last + AFTER).min(full.len());
    let cut = if cut < full.len() {
        memchr::memchr(b'\n', &full[cut..])
            .map(|k| cut + k + 1)
            .unwrap_or(full.len())
    } else {
        full.len()
    };
    let window = &full[start..cut];
    let t_lex = Instant::now();
    let lexed = lex(f.lang, window);
    let lex_us = t_lex.elapsed().as_micros();
    let t_ol = Instant::now();
    let mut ol = outline(f.lang, window, &lexed);
    if std::env::var_os("GREEG_DEBUG").is_some() {
        eprintln!(
            "refine {} full={} window={} read={}us lex={}us outline={}us defs={}",
            f.rel,
            full.len(),
            window.len(),
            read_us,
            lex_us,
            t_ol.elapsed().as_micros(),
            ol.defs.len()
        );
    }
    // shift window-relative offsets to absolute
    let base = start as u32;
    let line_base = source.line_of(base) - 1;
    for d in &mut ol.defs {
        d.start += base;
        d.end += base;
        d.name_start += base;
        d.name_end += base;
        d.line += line_base;
    }
    let src = full;
    let lexed = shift_lexed(lexed, base);
    let mut used: Vec<u32> = Vec::new();
    for h in &mut f.hits {
        if (h.match_start as usize) > src.len() {
            continue;
        }
        let ls = h.line_start as usize;
        let le = memchr::memchr(b'\n', &src[ls..])
            .map(|k| ls + k)
            .unwrap_or(src.len());
        let me = (h.match_end as usize).min(le) as u32;
        let (kind, di) = classify_full(
            f.lang,
            src,
            &lexed,
            &ol,
            h.match_start,
            me,
            h.line_start,
            &src[ls..le],
        );
        h.kind = kind;
        h.def_idx = di;
        if let Some(d) = di
            && !used.contains(&d)
        {
            used.push(d);
        }
    }
    used.sort_unstable();
    // Only the definitions the hits point at are materialized (chains included).
    let mut defs: Vec<DefSummary> = Vec::with_capacity(used.len());
    for &d in &used {
        let def = &ol.defs[d as usize];
        defs.push(DefSummary {
            name: String::from_utf8_lossy(&src[def.name_start as usize..def.name_end as usize])
                .into_owned(),
            kind: def.kind,
            line: def.line,
            start: def.start,
            end: def.end,
            chain: ol.chain(d, src),
            flags: 0,
        });
    }
    for h in &mut f.hits {
        if let Some(d) = h.def_idx {
            let i = used.binary_search(&d).ok().map(|i| i as u32);
            h.def_idx = i;
            h.chain = i
                .map(|i| defs[i as usize].chain.clone())
                .unwrap_or_default();
        }
    }
    f.defs = defs;
    f.refined = true;
}

fn shift_lexed(mut l: Lexed, base: u32) -> Lexed {
    for sp in &mut l.spans {
        sp.start += base;
        sp.end += base;
    }
    for b in &mut l.braces {
        b.off += base;
    }
    l
}

/// Refine several files, using a few scoped threads when there are many.
/// (No rayon: spinning up its global pool costs more than the work here.)
pub fn refine(r: &mut ScanResult, indices: &[usize]) {
    let t = Instant::now();
    let mut idx: Vec<usize> = indices.to_vec();
    idx.sort_unstable();
    idx.dedup();
    idx.retain(|&i| !r.files[i].refined);
    let mut taken: Vec<(usize, FileResult)> = idx
        .iter()
        .map(|&i| (i, std::mem::replace(&mut r.files[i], FileResult::empty())))
        .collect();
    let n = taken.len();
    if n <= 3 {
        for (_, f) in taken.iter_mut() {
            refine_file(f);
        }
    } else {
        let threads = n.min(4);
        let chunk = n.div_ceil(threads);
        std::thread::scope(|sc| {
            for part in taken.chunks_mut(chunk) {
                sc.spawn(move || {
                    for (_, f) in part.iter_mut() {
                        refine_file(f);
                    }
                });
            }
        });
    }
    for (i, f) in taken {
        r.files[i] = f;
    }
    r.stats.refine_ms += t.elapsed().as_secs_f64() * 1e3;
}

#[derive(Default)]
pub(crate) struct StatsAcc {
    pub(crate) searched: AtomicUsize,
    pub(crate) binary: AtomicUsize,
    pub(crate) huge: AtomicUsize,
    pub(crate) walked: AtomicUsize,
    pub(crate) matched: AtomicUsize,
    pub(crate) classify_ns: std::sync::atomic::AtomicU64,
    pub(crate) read_ns: std::sync::atomic::AtomicU64,
    pub(crate) search_ns: std::sync::atomic::AtomicU64,
}

fn scan_once(o: &Options, bounds: &ScanBounds) -> Result<ScanResult> {
    let t0 = Instant::now();
    let matcher = build_matcher(o)?;
    let threads = if o.threads == 0 {
        default_threads()
    } else {
        o.threads
    };
    let acc = StatsAcc::default();
    let classify =
        matches!(o.mode, Mode::Content | Mode::Outline | Mode::Block) || !o.kinds.is_empty();
    if classify {
        // compile the definition regexes while the walk starts
        std::thread::spawn(greeg_lang::defs::warm);
    }
    let cx = Ctx {
        o,
        matcher: &matcher,
        stats: &acc,
        classify,
        filter_kinds: true,
    };
    if o.use_index && !o.no_ignore && !o.hidden {
        // A panic anywhere in the index path degrades to scan mode:
        // the answer is still correct, one line goes to stderr, and the index is rebuilt.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            indexed::try_index(&cx, threads, t0)
        })) {
            Ok(Ok(Some(r))) => return Ok(r),
            Ok(Ok(None)) => {}
            Ok(Err(e)) => {
                if std::env::var_os("GREEG_DEBUG").is_some() {
                    eprintln!("greeg: index unavailable: {e:#}");
                }
            }
            Err(_) => {
                eprintln!(
                    "greeg: internal error in the index path; answering from a scan and rebuilding the index"
                );
                indexed::mark_corrupt(o);
            }
        }
    }
    let out: Mutex<Vec<FileResult>> = Mutex::new(Vec::new());
    let root = &o.root;
    /// Per-thread results, flushed when full and on drop (walker threads end without notice).
    struct Local<'a> {
        v: Vec<FileResult>,
        out: &'a Mutex<Vec<FileResult>>,
    }
    impl Drop for Local<'_> {
        fn drop(&mut self) {
            if !self.v.is_empty() {
                self.out.lock().unwrap().append(&mut self.v);
            }
        }
    }
    walker(o, threads, bounds)?.run(|| {
        let mut sb = SearcherBuilder::new();
        // bom_sniffing(false): offsets must index the buffer we read (the
        // searcher would otherwise strip a UTF-8 BOM and shift every offset by 3).
        // UTF-16 files are therefore searched as raw bytes (ripgrep transcodes them).
        sb.line_number(true)
            .binary_detection(BinaryDetection::quit(0))
            .multi_line(o.multiline)
            .bom_sniffing(false);
        let mut searcher = sb.build();
        let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
        let mut local = Local {
            v: Vec::new(),
            out: &out,
        };
        let cx = &cx;
        let bounds = *bounds;
        Box::new(move |entry| {
            let Ok(e) = entry else {
                return ignore::WalkState::Continue;
            };
            if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                return ignore::WalkState::Continue;
            }
            if let Some(d) = bounds.deadline
                && Instant::now() > d
            {
                return ignore::WalkState::Quit;
            }
            let p = e.path();
            cx.stats.walked.fetch_add(1, Relaxed);
            let rel = p
                .strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/");
            if let Some(fr) = process_file(cx, p, rel, &mut searcher, &mut buf) {
                let n = cx.stats.matched.fetch_add(1, Relaxed) + 1;
                local.v.push(fr);
                if local.v.len() >= 64 {
                    local.out.lock().unwrap().append(&mut local.v);
                }
                if bounds.max_matched > 0 && n >= bounds.max_matched {
                    return ignore::WalkState::Quit;
                }
            }
            ignore::WalkState::Continue
        })
    });
    let mut files = out.into_inner().unwrap();
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    let mut stats = Stats {
        files_walked: acc.walked.load(Relaxed),
        threads,
        source: "scan",
        ..Default::default()
    };
    finish_stats(&mut stats, &files, o, &acc, t0);
    Ok(ScanResult {
        opts: o.clone(),
        files,
        stats,
        rung: Rung::Exact,
        ignored_only: None,
        ignored_partial: false,
        related_index: Vec::new(),
    })
}

/// Finish a `Stats` from per-file results (shared by scan and index paths).
pub(crate) fn finish_stats(
    stats: &mut Stats,
    files: &[FileResult],
    o: &Options,
    acc: &StatsAcc,
    t0: Instant,
) {
    for f in files {
        stats.files_matched += 1;
        stats.total_hits += f.total;
        stats.total_unfiltered += f.total_unfiltered;
        for k in 0..9 {
            stats.by_kind[k] += f.kinds[k] as usize;
        }
        if f.flags.demoted() && !o.all {
            stats.demoted_files += 1;
            stats.demoted_hits += f.total;
        }
        if f.flags.has(FileFlags::MINIFIED) {
            stats.minified_hits += f.total;
        }
    }
    stats.files_searched = acc.searched.load(Relaxed);
    stats.skipped_binary = acc.binary.load(Relaxed);
    stats.skipped_huge = acc.huge.load(Relaxed);
    stats.elapsed_ms = t0.elapsed().as_secs_f64() * 1e3;
    stats.cpu_read_ms = acc.read_ns.load(Relaxed) as f64 / 1e6;
    stats.cpu_search_ms = acc.search_ns.load(Relaxed) as f64 / 1e6;
    stats.cpu_classify_ms = acc.classify_ns.load(Relaxed) as f64 / 1e6;
}

/// Is the pattern a bare identifier (so name-based rungs apply)?
fn is_plain_word(p: &str) -> bool {
    !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub(crate) fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if !c.is_alphanumeric() && c != '_' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Does the path argument look like a glob (`*`, `?`, `[`)?
pub fn looks_like_glob(p: &Path) -> bool {
    p.to_string_lossy()
        .bytes()
        .any(|b| matches!(b, b'*' | b'?' | b'['))
}

/// C13: a positional path with glob metacharacters that does not exist on disk
/// is a `-g GLOB`. Returns the rewritten options when any path was converted.
pub fn normalize_paths(o: &Options) -> Option<Options> {
    if !o.paths.iter().any(|p| looks_like_glob(p) && !p.exists()) {
        return None;
    }
    let mut o2 = o.clone();
    o2.paths.clear();
    for p in &o.paths {
        if looks_like_glob(p) && !p.exists() {
            o2.globs
                .push(p.to_string_lossy().trim_start_matches("./").to_string());
        } else {
            o2.paths.push(p.clone());
        }
    }
    Some(o2)
}

/// Search under the selected policy; only discovery may climb the escalation ladder.
pub fn scan(o: &Options) -> Result<ScanResult> {
    let normalized = normalize_paths(o);
    let o = normalized.as_ref().unwrap_or(o);
    if let Some(p) = o.paths.iter().find(|p| !p.exists()) {
        // ripgrep reports a missing path and exits 2
        anyhow::bail!("{}: No such file or directory (os error 2)", p.display());
    }
    let plain = ScanBounds::default();
    let mut r = scan_once(o, &plain)?;
    if r.stats.total_hits > 0 || o.matching == MatchingPolicy::Exact {
        return Ok(r);
    }
    let mut elapsed = r.stats.elapsed_ms;
    if o.word {
        let mut o2 = o.clone();
        o2.word = false;
        let mut r2 = scan_once(&o2, &plain)?;
        elapsed += r2.stats.elapsed_ms;
        if r2.stats.total_hits > 0 {
            r2.rung = Rung::NoWord;
            r2.stats.elapsed_ms = elapsed;
            return Ok(r2);
        }
        r = r2;
    }
    if !o.case_insensitive {
        let mut o2 = o.clone();
        o2.case_insensitive = true;
        o2.word = false;
        let mut r2 = scan_once(&o2, &plain)?;
        elapsed += r2.stats.elapsed_ms;
        if r2.stats.total_hits > 0 {
            r2.rung = Rung::CaseInsensitive;
            r2.stats.elapsed_ms = elapsed;
            return Ok(r2);
        }
        r = r2;
    }
    // rungs 3 and 4 need the symbol names
    if o.use_index && !o.no_ignore && !o.hidden && (o.fixed_strings || is_plain_word(&o.pattern)) {
        let threads = if o.threads == 0 {
            default_threads()
        } else {
            o.threads
        };
        if let Ok(Some(op)) = indexed::open_fresh(o, threads)
            && op.idx.has_symbols()
        {
            let toks = greeg_index::symtab::split_tokens(&o.pattern);
            let mut alts: Vec<String> = Vec::new();
            let mut rung = Rung::Exact;
            if toks.len() >= 2 {
                alts = op.idx.names_with_tokens(&toks, 8);
                if !alts.is_empty() {
                    rung = Rung::SplitTokens(alts.clone());
                }
            }
            if alts.is_empty() && o.pattern.len() >= 4 {
                let d = if o.pattern.len() < 8 { 1 } else { 2 };
                alts = op
                    .idx
                    .fuzzy_names(&o.pattern, d, 6)
                    .into_iter()
                    .map(|(n, _)| n)
                    .collect();
                if !alts.is_empty() {
                    rung = Rung::Fuzzy(alts.clone());
                }
            }
            drop(op);
            if !alts.is_empty() {
                let mut o2 = o.clone();
                o2.pattern = alts
                    .iter()
                    .map(|a| regex_escape(a))
                    .collect::<Vec<_>>()
                    .join("|");
                o2.fixed_strings = false;
                o2.word = true;
                o2.case_insensitive = false;
                let mut r2 = scan_once(&o2, &plain)?;
                elapsed += r2.stats.elapsed_ms;
                if r2.stats.total_hits > 0 {
                    r2.rung = rung;
                    r2.stats.elapsed_ms = elapsed;
                    r2.opts = o.clone();
                    return Ok(r2);
                }
            }
        }
    }
    // rung 5: ignored/hidden files, counted only, bounded (never `.git/`, 50 ms, 200 files)
    if !o.no_ignore || !o.hidden {
        let mut o2 = o.clone();
        o2.no_ignore = true;
        o2.hidden = true;
        o2.mode = Mode::Count;
        o2.budget = 0;
        o2.use_index = false;
        let bounds = ScanBounds {
            deadline: Some(Instant::now() + Duration::from_millis(50)),
            skip_git: true,
            max_matched: 200,
        };
        let r2 = scan_once(&o2, &bounds)?;
        elapsed += r2.stats.elapsed_ms;
        if r2.stats.total_hits > 0 {
            r.ignored_only = Some((r2.stats.files_matched, r2.stats.total_hits));
            r.ignored_partial =
                r2.stats.files_matched >= bounds.max_matched || r2.stats.elapsed_ms >= 50.0;
            r.rung = Rung::Ignored;
        }
    }
    r.stats.elapsed_ms = elapsed;
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pattern: &str) -> Options {
        Options {
            pattern: pattern.to_string(),
            ..Default::default()
        }
    }

    fn search(
        o: &Options,
        src: &[u8],
        lang: Lang,
        cap: usize,
        keep_defs: bool,
    ) -> CollectSink<'static> {
        let m: &'static RegexMatcher = Box::leak(Box::new(build_matcher(o).unwrap()));
        let mut sink = CollectSink::new(m, lang, cap, keep_defs, o.multiline);
        let mut sb = SearcherBuilder::new();
        sb.line_number(true)
            .binary_detection(BinaryDetection::quit(0))
            .multi_line(o.multiline)
            .bom_sniffing(false);
        sb.build().search_slice(m, src, &mut sink).unwrap();
        sink
    }

    #[test]
    fn clip_line_short_and_long() {
        let (t, c, m) = clip_line(b"let x = foo();\r", 8, 11, 200);
        assert_eq!(t, b"let x = foo();");
        assert!(!c);
        assert_eq!(m, (8, 11));
        let long: Vec<u8> = (0..300).map(|i| b'a' + (i % 26) as u8).collect();
        let (t, c, (ms, me)) = clip_line(&long, 150, 153, 40);
        assert!(c);
        assert!(t.starts_with("…".as_bytes()) && t.ends_with("…".as_bytes()));
        assert_eq!(&t[ms..me], &long[150..153]);
        assert!(t.len() <= 40 + 2 * "…".len());
        // match at the start: no leading marker
        let (t, _, (ms, _)) = clip_line(&long, 0, 3, 40);
        assert!(!t.starts_with("…".as_bytes()));
        assert_eq!(ms, 0);
        // zero means never clip
        let (t, c, _) = clip_line(&long, 0, 3, 0);
        assert_eq!(t.len(), 300);
        assert!(!c);
    }

    #[test]
    fn sink_groups_submatches_per_line() {
        let src = b"foo foo\nbar\nfoo\n";
        let s = search(&opts("foo"), src, Lang::Rust, 64, false);
        assert_eq!(s.total, 2);
        assert_eq!(s.hits.len(), 2);
        assert_eq!(
            s.hits[0],
            LineHit {
                line: 1,
                line_start: 0,
                subs: vec![(0, 3), (4, 7)]
            }
        );
        assert_eq!(
            s.hits[1],
            LineHit {
                line: 3,
                line_start: 12,
                subs: vec![(12, 15)]
            }
        );
    }

    #[test]
    fn sink_multiline_records_each_match_line() {
        let src = b"a\nrespond(\nx)\nfoo\nrespond(\ny\n";
        let mut o = opts(r"respond\(\n");
        o.multiline = true;
        let s = search(&o, src, Lang::Kotlin, 64, false);
        assert_eq!(s.total, 2);
        assert_eq!(
            s.hits.iter().map(|h| h.line).collect::<Vec<_>>(),
            vec![2, 5]
        );
        assert_eq!(s.hits[0].line_start, 2);
        assert_eq!(s.hits[0].subs, vec![(2, 11)]);
        assert_eq!(s.hits[1].line_start, 18);
        // a block spanning three lines with two matches on different lines
        let src = b"x1\nab\ncd\nx2\nab\n";
        let mut o = opts(r"(?s)ab\ncd|x\d");
        o.multiline = true;
        let s = search(&o, src, Lang::Rust, 64, false);
        assert_eq!(
            s.hits.iter().map(|h| h.line).collect::<Vec<_>>(),
            vec![1, 2, 4]
        );
        assert_eq!(s.total, 3);
    }

    #[test]
    fn sink_keeps_definitions_beyond_cap() {
        let mut src = Vec::new();
        for _ in 0..70 {
            src.extend_from_slice(b"    spawn();\n");
        }
        src.extend_from_slice(b"pub fn spawn() {}\n");
        for _ in 0..3 {
            src.extend_from_slice(b"    spawn();\n");
        }
        let s = search(&opts("spawn"), &src, Lang::Rust, 64, true);
        assert_eq!(s.total, 74);
        assert_eq!(s.hits.len(), 65);
        assert_eq!(s.hits[64].line, 71);
        assert_eq!(s.defs_kept, 1);
        // without def retention the definition is lost
        let s = search(&opts("spawn"), &src, Lang::Rust, 64, false);
        assert_eq!(s.hits.len(), 64);
    }

    #[test]
    fn sink_first_only_stops() {
        let src = b"foo\nfoo\nfoo\n";
        let m: &'static RegexMatcher = Box::leak(Box::new(build_matcher(&opts("foo")).unwrap()));
        let mut sink = CollectSink::new(m, Lang::Rust, 64, false, false);
        sink.first_only = true;
        let mut sb = SearcherBuilder::new();
        sb.line_number(true);
        sb.build().search_slice(m, src, &mut sink).unwrap();
        assert_eq!(sink.total, 1);
        assert_eq!(sink.hits.len(), 1);
    }

    fn tmp_file(name: &str, content: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("greeg-query-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, content).unwrap();
        p
    }

    fn run_process(o: &Options, path: &Path) -> Option<FileResult> {
        let matcher = build_matcher(o).unwrap();
        let acc = StatsAcc::default();
        let cx = Ctx {
            o,
            matcher: &matcher,
            stats: &acc,
            classify: true,
            filter_kinds: true,
        };
        let mut sb = SearcherBuilder::new();
        sb.line_number(true)
            .binary_detection(BinaryDetection::quit(0))
            .multi_line(o.multiline)
            .bom_sniffing(false);
        let mut searcher = sb.build();
        let mut buf = Vec::new();
        process_file(
            &cx,
            path,
            path.file_name().unwrap().to_string_lossy().to_string(),
            &mut searcher,
            &mut buf,
        )
    }

    #[test]
    fn kind_filter_totals_are_consistent() {
        let mut src = Vec::new();
        for _ in 0..70 {
            src.extend_from_slice(b"    spawn();\n");
        }
        src.extend_from_slice(b"pub fn spawn() {}\n");
        let p = tmp_file("kinds.rs", &src);
        let mut o = opts("spawn");
        o.kinds = vec![HitKind::Def];
        let f = run_process(&o, &p).unwrap();
        assert_eq!(f.total, 1);
        assert_eq!(f.total_unfiltered, 71);
        assert_eq!(f.hits.len(), 1);
        assert_eq!(f.hits[0].kind, HitKind::Def);
        assert_eq!(f.hits[0].line, 71);
        assert!(
            (f.hits[0].score - 1.0 * 0.8 * 1.3).abs() < 1e-5,
            "exact definitions get 1.3x"
        );
        let o = opts("spawn");
        let f = run_process(&o, &p).unwrap();
        assert_eq!(f.total, 71);
        assert_eq!(f.hits.len(), 65);
    }

    #[test]
    fn bom_is_not_part_of_the_first_line() {
        let p = tmp_file("bom.rs", b"\xEF\xBB\xBFfoo bar\nfoo\n");
        let o = opts("^foo");
        let f = run_process(&o, &p).unwrap();
        assert_eq!(f.total, 2);
        assert_eq!(f.hits[0].line, 1);
        assert_eq!(f.hits[0].raw, b"foo bar");
        assert_eq!(f.hits[0].column(), 0);
        assert_eq!(f.hits[0].line_start, 3);
    }

    #[test]
    fn nul_after_8k_counts_as_binary() {
        let mut src = vec![b'a'; 9000];
        src.extend_from_slice(b"\nfoo\n\0\n");
        let p = tmp_file("bin.txt", &src);
        let o = opts("foo");
        let matcher = build_matcher(&o).unwrap();
        let acc = StatsAcc::default();
        let cx = Ctx {
            o: &o,
            matcher: &matcher,
            stats: &acc,
            classify: false,
            filter_kinds: true,
        };
        let mut sb = SearcherBuilder::new();
        sb.line_number(true)
            .binary_detection(BinaryDetection::quit(0))
            .bom_sniffing(false);
        let mut searcher = sb.build();
        let mut buf = Vec::new();
        assert!(process_file(&cx, &p, "bin.txt".into(), &mut searcher, &mut buf).is_none());
        assert_eq!(acc.binary.load(Relaxed), 1);
    }

    #[test]
    fn multiline_hits_survive_process_file() {
        let p = tmp_file("ml.kt", b"call.respond(\n    x)\nfoo\n");
        let mut o = opts(r"respond\(\n");
        o.multiline = true;
        let f = run_process(&o, &p).unwrap();
        assert_eq!(f.total, 1);
        assert_eq!(f.hits[0].line, 1);
        assert_eq!(f.hits[0].raw, b"call.respond(");
        assert_eq!(f.hits[0].kind, HitKind::Call);
    }

    #[test]
    fn mock_paths_are_demoted() {
        assert!(is_mock_path("tokio/src/fs/mocks.rs"));
        assert!(is_mock_path("src/__mocks__/fs.ts"));
        assert!(is_mock_path("lib/stubs/client.py"));
        assert!(is_mock_path("test/Fakes/FakeClock.kt"));
        assert!(!is_mock_path("src/mockingbird.rs"));
        assert!(!is_mock_path("src/runtime/task/join.rs"));
        assert_eq!(
            loc_weight(FileFlags::default(), "tokio/src/fs/mocks.rs", false),
            0.45
        );
        assert_eq!(
            loc_weight(FileFlags::default(), "tokio/src/fs/mocks.rs", true),
            1.0
        );
    }

    #[test]
    fn glob_paths_become_globs() {
        let mut o = opts("x");
        o.paths = vec![PathBuf::from("django/db/**"), PathBuf::from(".")];
        let n = normalize_paths(&o).unwrap();
        assert_eq!(n.globs, vec!["django/db/**".to_string()]);
        assert_eq!(n.paths, vec![PathBuf::from(".")]);
        assert!(normalize_paths(&opts("x")).is_none());
    }

    #[test]
    fn source_lines() {
        let s = Source::new(b"a\nbb\r\nccc".to_vec());
        assert_eq!(s.line_count(), 3);
        assert_eq!(s.line(1), Some(&b"a"[..]));
        assert_eq!(s.line(2), Some(&b"bb"[..]));
        assert_eq!(s.line(3), Some(&b"ccc"[..]));
        assert_eq!(s.line(4), None);
        assert_eq!(s.line_of(0), 1);
        assert_eq!(s.line_of(2), 2);
        assert_eq!(s.line_of(6), 3);
        assert_eq!(s.lines(2, 9).1.len(), 2);
        assert_eq!(s.end_line(0, 6), 2);
        assert_eq!(s.end_line(2, 9), 3);
    }

    /// (snippet, match, expected kind); the match is the first occurrence of `m`.
    fn check_table(lang: Lang, table: &[(&str, &str, HitKind)]) {
        let mut bad = Vec::new();
        for (line, m, want) in table {
            let ms = line.find(m).expect("match in snippet");
            let me = ms + m.len();
            let got = classify_line(lang, line.as_bytes(), ms, me);
            if got != *want {
                bad.push(format!(
                    "{lang:?} {line:?} match {m:?}: got {got:?}, want {want:?}"
                ));
            }
        }
        assert!(bad.is_empty(), "\n{}", bad.join("\n"));
    }

    #[test]
    fn byte_rules_rust() {
        use HitKind::*;
        check_table(
            Lang::Rust,
            &[
                ("let mut count = 0;", "count", Ident),
                ("let x = &count;", "count", Ident),
                ("let m = a & b;", "b", Ident),
                ("match state {", "state", Ident),
                ("for x in items {", "items", Ident),
                ("while running {", "running", Ident),
                ("if ready {", "ready", Ident),
                ("if let Some(x) = self.next {", "next", Member),
                ("return Config { a: 1 };", "Config", Call),
                ("let c = Config { a: 1 };", "Config", Call),
                ("foo(Config { a: 1 })", "Config", Call),
                ("fn f(x: &Semaphore) -> Handle {", "Semaphore", Type),
                ("fn f(x: &Semaphore) -> Handle {", "Handle", Type),
                ("fn f(x: &'a mut Semaphore)", "Semaphore", Type),
                ("let s: Arc<Semaphore> = x;", "Semaphore", Type),
                ("impl Future for JoinHandle<T> {", "Future", Type),
                ("fn f(x: T) -> JoinHandle<T> {", "JoinHandle", Type),
                ("let v: Vec<u8> = Vec::new();", "new", Call),
                ("Semaphore::new(1)", "Semaphore", Type),
                ("let x = self.sem.acquire();", "sem", Member),
                ("if i<n {", "n", Ident),
                ("Box<dyn Error>", "Error", Type),
                ("x as usize", "usize", Type),
                ("use crate::sync::Semaphore;", "Semaphore", Import),
                ("pub struct Semaphore {", "Semaphore", Def),
                ("// a Semaphore", "Semaphore", Comment),
                ("let x = *const Foo;", "Foo", Ident),
                ("fn f(p: *const Foo)", "Foo", Type),
            ],
        );
    }

    #[test]
    fn byte_rules_kotlin() {
        use HitKind::*;
        check_table(
            Lang::Kotlin,
            &[
                ("class Foo : Bar {", "Bar", Type),
                ("class Foo(val x: Int) : Bar, Baz {", "Baz", Type),
                (
                    "fun f(call: ApplicationCall): Response {",
                    "ApplicationCall",
                    Type,
                ),
                ("fun f(call: ApplicationCall): Response {", "Response", Type),
                ("val x = Foo {", "Foo", Call),
                ("launch {", "launch", Call),
                ("list.forEach {", "forEach", Call),
                ("call.respond(x)", "respond", Call),
                ("call.respond(x)", "call", Ident),
                ("val client = HttpClient(CIO)", "HttpClient", Call),
                ("if (x is Foo) {", "Foo", Type),
                ("val y = x as Foo", "Foo", Type),
                ("when (state) {", "state", Ident),
                ("for (item in items) {", "items", Ident),
                ("val m: Map<String, Foo> = mapOf()", "Foo", Type),
                ("object : Runnable {", "Runnable", Type),
                (
                    "import io.ktor.server.application.ApplicationCall",
                    "ApplicationCall",
                    Import,
                ),
                ("val x = config.host", "host", Member),
                ("else {", "else", Ident),
            ],
        );
    }

    #[test]
    fn byte_rules_typescript() {
        use HitKind::*;
        check_table(
            Lang::TypeScript,
            &[
                ("class Foo extends Bar implements Baz {", "Bar", Type),
                ("class Foo extends Bar implements Baz {", "Baz", Type),
                ("const x = { key: value };", "value", Ident),
                ("const x = { key: value };", "key", Member),
                (
                    "return { node: createSourceFile(x) };",
                    "createSourceFile",
                    Call,
                ),
                ("return { node: sourceFile };", "sourceFile", Ident),
                (
                    "function f(node: Node, flags: ParseFlags): SourceFile {",
                    "Node",
                    Type,
                ),
                (
                    "function f(node: Node, flags: ParseFlags): SourceFile {",
                    "ParseFlags",
                    Type,
                ),
                (
                    "function f(node: Node, flags: ParseFlags): SourceFile {",
                    "SourceFile",
                    Type,
                ),
                ("let x: Map<string, Node> = new Map();", "Node", Type),
                ("const s = new SourceFile(x);", "SourceFile", Call),
                ("if (x instanceof Node) {", "Node", Type),
                ("const y = x as Node;", "Node", Type),
                ("readonly kind: SyntaxKind;", "SyntaxKind", Type),
                ("readonly kind: SyntaxKind;", "kind", Member),
                ("node.parent = x;", "parent", Member),
                ("if (i<n) {", "n", Ident),
                ("for (const x of nodes) {", "nodes", Ident),
                ("import { Node } from './types';", "Node", Import),
                (
                    "export function createSourceFile(x) {",
                    "createSourceFile",
                    Def,
                ),
                ("foo<T>(x)", "foo", Call),
                ("const t = typeof node;", "node", Type),
            ],
        );
    }

    #[test]
    fn byte_rules_javascript() {
        use HitKind::*;
        check_table(
            Lang::JavaScript,
            &[
                ("const x = { key: value };", "value", Ident),
                ("const x = { key: value };", "key", Member),
                ("class Foo extends Bar {", "Bar", Type),
                ("const s = new Server(x);", "Server", Call),
                ("if (x instanceof Node) {", "Node", Type),
                ("const t = typeof node;", "node", Ident),
                ("node.parent = x;", "parent", Member),
                ("return render(x);", "render", Call),
                ("return value;", "value", Ident),
                ("for (const x of nodes) {", "nodes", Ident),
                ("const { a, b } = props;", "props", Ident),
                ("case Foo:", "Foo", Ident),
                ("module.exports = { handler: handler };", "handler", Member),
                ("const fs = require('fs');", "require", Import),
            ],
        );
    }

    #[test]
    fn byte_rules_python() {
        use HitKind::*;
        check_table(
            Lang::Python,
            &[
                ("class Foo(Base):", "Base", Type),
                ("class Foo(Base, Mixin):", "Mixin", Type),
                (
                    "def get(self, request: HttpRequest) -> HttpResponse:",
                    "HttpRequest",
                    Type,
                ),
                (
                    "def get(self, request: HttpRequest) -> HttpResponse:",
                    "HttpResponse",
                    Type,
                ),
                ("x = {'key': value}", "value", Ident),
                ("x = {\"key\": Value}", "Value", Ident),
                ("x: Optional[Model] = None", "Optional", Type),
                ("x: Optional[Model] = None", "Model", Type),
                ("items: list[Item] = []", "Item", Type),
                ("qs = self.get_queryset()", "get_queryset", Call),
                ("qs = self.queryset", "queryset", Member),
                ("if x in items:", "items", Ident),
                ("for item in items:", "items", Ident),
                ("return HttpResponse(x)", "HttpResponse", Call),
                ("return response", "response", Ident),
                ("except ValueError:", "ValueError", Type),
                ("except (ValueError, TypeError):", "TypeError", Type),
                ("from django.db import models", "models", Import),
                ("def get_queryset(self):", "get_queryset", Def),
                ("if x is None:", "None", Ident),
                ("with open(f) as fh:", "fh", Ident),
                ("lambda x: x + 1", "x", Ident),
            ],
        );
    }
}
