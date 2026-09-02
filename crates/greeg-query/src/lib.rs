//! Scan-mode search: walk, match with ripgrep's crates, classify each hit with
//! the language layer, score it, and hand a `ScanResult` to `shape`.

pub mod indexed;
pub mod precise;
pub mod session;
pub mod shape;
pub mod tokens;
pub mod verbs;

use anyhow::{Context, Result};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use greeg_lang::defs::{Outline, outline};
use greeg_lang::lexer::{Lexed, SpanKind, lex};
use greeg_lang::{DefKind, FileFlags, Lang, content_flags, is_import_line, path_flags};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Instant, SystemTime};

pub const MAX_HITS_PER_FILE: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Files,
    Count,
    Outline,
    Content,
    Block,
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
    pub ladder: bool,
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
            ladder: true,
            max_columns: 200,
            per_file_cap: 4,
            context: None,
            before: 0,
            after: 0,
            use_index: true,
            fresh: greeg_index::fresh::Mode::Auto,
            index_dir: None,
            precise: false,
        }
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
    pub const ALL: [HitKind; 9] = [HitKind::Def, HitKind::Import, HitKind::Call, HitKind::Type, HitKind::Member, HitKind::Ident, HitKind::Docstring, HitKind::Comment, HitKind::Str];
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
    pub fn weight(self) -> f32 {
        match self {
            HitKind::Def => 1.0,
            HitKind::Import => 0.6,
            HitKind::Call => 0.55,
            HitKind::Type => 0.5,
            HitKind::Member => 0.45,
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
    pub line: u32,
    /// Absolute byte offset of the line start.
    pub line_start: u32,
    /// Absolute byte offset of the match.
    pub match_start: u32,
    pub match_end: u32,
    pub kind: HitKind,
    /// Enclosing definition chain, outermost first.
    pub chain: Vec<(DefKind, String)>,
    pub def_idx: Option<u32>,
    pub score: f32,
    /// Line text without the terminator, clipped to `max_columns` around the match.
    pub text: Vec<u8>,
    /// Match range within `text` (after trimming and clipping).
    pub text_match: (u32, u32),
    pub clipped: bool,
}

#[derive(Clone, Debug)]
pub struct FileResult {
    pub rel: String,
    pub path: PathBuf,
    pub lang: Lang,
    pub flags: FileFlags,
    pub size: u64,
    pub age_days: f32,
    pub prior: f32,
    pub hits: Vec<Hit>,
    /// Total matches in the file (hits beyond MAX_HITS_PER_FILE are counted only).
    pub total: usize,
    pub kinds: [u32; 9],
    /// Definitions found in the file (only computed when classification ran).
    pub defs: Vec<DefSummary>,
    /// Kinds, chains and `defs` are final (from the index); `refine` skips the file.
    pub refined: bool,
    /// Index file id when the result came from the index.
    pub file_id: Option<u32>,
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
    pub total_hits: usize,
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
}

pub(crate) fn default_threads() -> usize {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    if cfg!(target_os = "macos") { cores.min(4) } else { cores }
}

pub(crate) fn build_matcher(o: &Options) -> Result<RegexMatcher> {
    let mut b = RegexMatcherBuilder::new();
    b.case_insensitive(o.case_insensitive).case_smart(o.smart_case && !o.case_insensitive).word(o.word).multi_line(true).fixed_strings(o.fixed_strings).whole_line(o.line_regexp);
    if !o.multiline {
        b.line_terminator(Some(b'\n'));
    }
    b.build(&o.pattern).with_context(|| format!("invalid pattern {:?}", o.pattern))
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
    tb.build().map_err(|e| anyhow::anyhow!("unrecognized file type: {e}"))
}


fn walker(o: &Options, threads: usize) -> Result<ignore::WalkParallel> {
    let mut roots: Vec<PathBuf> = if o.paths.is_empty() { vec![o.root.clone()] } else { o.paths.clone() };
    roots.dedup();
    let mut wb = ignore::WalkBuilder::new(&roots[0]);
    for r in &roots[1..] {
        wb.add(r);
    }
    wb.hidden(!o.hidden).git_ignore(!o.no_ignore).git_global(!o.no_ignore).git_exclude(!o.no_ignore).ignore(!o.no_ignore).parents(!o.no_ignore).threads(threads);
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

/// Sink that records up to MAX_HITS_PER_FILE matches and counts the rest.
struct CollectSink<'a> {
    matcher: &'a RegexMatcher,
    hits: Vec<(u64, u32, u32, u32)>, // (line, line_start, match_start, match_end) absolute
    total: usize,
    max_per_line: usize,
    cap: usize,
}

impl Sink for CollectSink<'_> {
    type Error = io::Error;
    fn matched(&mut self, _s: &Searcher, m: &SinkMatch<'_>) -> Result<bool, io::Error> {
        let line_no = m.line_number().unwrap_or(0);
        let base = m.absolute_byte_offset() as u32;
        let bytes = m.bytes();
        let mut n = 0;
        let _ = self.matcher.find_iter(bytes, |mat| {
            self.total += 1;
            if self.hits.len() < self.cap {
                self.hits.push((line_no, base, base + mat.start() as u32, base + mat.end() as u32));
            }
            n += 1;
            n < self.max_per_line
        });
        Ok(true)
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
        let ndir = if Path::new(n).extension().is_some() { n.rsplit_once('/').map(|(d, _)| d).unwrap_or("") } else { n };
        if rel.starts_with(n) || dir == ndir || (ndir.is_empty() && !rel.contains('/')) {
            return 1.0;
        }
        let parent = |d: &str| d.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
        if parent(dir) == parent(ndir) {
            best = best.max(0.85);
        }
    }
    best
}

pub(crate) fn file_prior(flags: FileFlags, age_days: f32, rel: &str, o: &Options) -> f32 {
    let loc = if o.all {
        1.0
    } else if flags.has(FileFlags::MINIFIED) {
        0.1
    } else if flags.has(FileFlags::GENERATED | FileFlags::VENDORED | FileFlags::LOCKFILE) {
        0.2
    } else if flags.has(FileFlags::TEST) {
        0.45
    } else {
        1.0
    };
    let recency = if age_days < 1.0 { 1.0 } else if age_days < 30.0 { 0.95 } else { 0.9 };
    loc * 0.8 * near_weight(rel, &o.near) * recency
}

/// Byte-context rule for kinds that are not stored as spans (DESIGN.md §6.2).
/// `src` may be the whole file or just the hit line; offsets are relative to it.
pub(crate) fn kind_by_context(lang: Lang, src: &[u8], ms: usize, me: usize, line_start: usize, line: &[u8]) -> HitKind {
    if is_import_line(lang, line) {
        return HitKind::Import;
    }
    let after = {
        let mut i = me;
        while i < src.len() && (src[i] == b' ' || src[i] == b'\t') {
            i += 1;
        }
        src.get(i).copied().unwrap_or(0)
    };
    let before_ws = {
        let mut i = ms;
        while i > line_start && (src[i - 1] == b' ' || src[i - 1] == b'\t') {
            i -= 1;
        }
        &src[line_start..i]
    };
    let ends_with = |s: &[u8]| before_ws.ends_with(s);
    let prev = if ms > 0 { src[ms - 1] } else { 0 };
    // Rust reference/pointer types and lifetimes: `&T`, `&'a T`, `&mut T`, `*const T`
    let ref_type = lang == Lang::Rust && (ends_with(b"&") || ends_with(b"mut") || ends_with(b"const") || (before_ws.len() >= 2 && before_ws[before_ws.len() - 2] == b'\'' && before_ws.last().map(|b| b.is_ascii_alphabetic()).unwrap_or(false)) || {
        // `&'a T`: before_ws ends with a lifetime name
        let t = before_ws;
        let mut i = t.len();
        while i > 0 && (t[i - 1].is_ascii_alphanumeric() || t[i - 1] == b'_') {
            i -= 1;
        }
        i > 0 && t[i - 1] == b'\'' && i < t.len()
    });
    // inside a generic argument list: `<A, Semaphore>` (no call parenthesis in between)
    let in_generics = matches!(lang, Lang::Rust | Lang::TypeScript | Lang::Kotlin) && (after == b'>' || after == b',') && {
        let head = &src[line_start..ms];
        match (memchr::memrchr(b'<', head), memchr::memrchr(b'(', head)) {
            (Some(lt), Some(lp)) => lt > lp,
            (Some(_), None) => true,
            _ => false,
        }
    };
    let trailing_lambda = matches!(lang, Lang::Kotlin | Lang::Rust) && after == b'{' && src.get(me).copied() == Some(b' ');
    if after == b'(' || trailing_lambda || ends_with(b"new") || ends_with(b"await new") {
        HitKind::Call
    } else if ref_type || in_generics || (after == b'<' && matches!(lang, Lang::TypeScript | Lang::Rust | Lang::Kotlin)) || ends_with(b":") || ends_with(b"->") || ends_with(b"impl") || ends_with(b"extends") || ends_with(b"implements") || ends_with(b"is") || ends_with(b"as") || ends_with(b"dyn") || ends_with(b"struct") || ends_with(b"trait") || prev == b'<' || prev == b'&' || ends_with(b"instanceof") || ends_with(b"typeof") {
        HitKind::Type
    } else if prev == b'.' || (prev == b':' && ms >= 2 && src[ms - 2] == b':') {
        HitKind::Member
    } else {
        HitKind::Ident
    }
}

/// Cheap, line-local classification used during the scan (no file outline).
fn classify_line(lang: Lang, line: &[u8], ms: usize, me: usize) -> HitKind {
    if !lang.has_grammar() {
        return HitKind::Ident;
    }
    // a line without quote or comment starters has no noncode spans: skip the lexer
    if memchr::memchr3(b'"', b'\'', b'/', line).is_some() || memchr::memchr2(b'#', b'`', line).is_some() {
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
        && ns < me && ms < ne {
            return HitKind::Def;
        }
    kind_by_context(lang, line, ms, me, 0, line)
}

/// Precise classification with a file outline (used for shown files).
#[allow(clippy::too_many_arguments)]
fn classify_full(lang: Lang, src: &[u8], lexed: &Lexed, ol: &Outline, ms: u32, me: u32, line_start: u32, line: &[u8]) -> (HitKind, Option<u32>) {
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
    (kind_by_context(lang, src, ms as usize, me as usize, line_start as usize, line), ol.enclosing(ms))
}

pub(crate) fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

pub(crate) struct Ctx<'a> {
    pub(crate) o: &'a Options,
    pub(crate) matcher: &'a RegexMatcher,
    pub(crate) stats: &'a StatsAcc,
    pub(crate) classify: bool,
    /// Apply `--kind` inside `process_file` (false when the index classifies afterwards).
    pub(crate) filter_kinds: bool,
}

pub(crate) fn process_file(cx: &Ctx, path: &Path, rel: String, searcher: &mut Searcher, buf: &mut Vec<u8>) -> Option<FileResult> {
    let o = cx.o;
    cx.stats.searched.fetch_add(1, Relaxed);
    // Path-derived exclusions are cheap enough to apply before reading.
    let mut flags = FileFlags::default();
    let need_path_flags = !o.all && (o.no_tests || o.no_vendored || o.no_generated);
    if need_path_flags {
        flags = path_flags(&rel);
        if o.no_tests && flags.has(FileFlags::TEST) || o.no_vendored && flags.has(FileFlags::VENDORED) || o.no_generated && flags.has(FileFlags::GENERATED | FileFlags::MINIFIED | FileFlags::LOCKFILE) {
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
    cx.stats.read_ns.fetch_add(t_read.elapsed().as_nanos() as u64, Relaxed);
    if buf.len() as u64 > o.max_filesize {
        cx.stats.huge.fetch_add(1, Relaxed);
        return None;
    }
    greeg_lang::transcode_utf16(buf);
    let src: &[u8] = buf;
    if memchr::memchr(0, &src[..src.len().min(8192)]).is_some() {
        cx.stats.binary.fetch_add(1, Relaxed);
        return None;
    }
    let mut sink = CollectSink { matcher: cx.matcher, hits: Vec::new(), total: 0, max_per_line: 8, cap: if o.budget == 0 { usize::MAX } else { MAX_HITS_PER_FILE } };
    let t_search = Instant::now();
    let r = searcher.search_slice(cx.matcher, src, &mut sink);
    cx.stats.search_ns.fetch_add(t_search.elapsed().as_nanos() as u64, Relaxed);
    if r.is_err() || sink.total == 0 {
        return None;
    }
    let t_cls = Instant::now();
    // Only matched files pay for flags, metadata and classification.
    if !need_path_flags {
        flags = path_flags(&rel);
    }
    flags.0 |= content_flags(&src[..src.len().min(65536)], src.len() as u64).0;
    if flags.has(FileFlags::BINARY) {
        cx.stats.binary.fetch_add(1, Relaxed);
        return None;
    }
    let md = fs::metadata(path).ok();
    let size = md.as_ref().map(|m| m.len()).unwrap_or(src.len() as u64);
    let mtime = md.and_then(|m| m.modified().ok());
    let lang = Lang::from_path(path);
    let age_days = mtime.and_then(|m| SystemTime::now().duration_since(m).ok()).map(|d| d.as_secs_f32() / 86400.0).unwrap_or(365.0);
    let prior = file_prior(flags, age_days, &rel, o);
    let pat = o.pattern.as_bytes();
    let mut hits = Vec::with_capacity(sink.hits.len());
    let mut kinds = [0u32; 9];
    for (line, ls, ms, me) in sink.hits {
        let le = memchr::memchr(b'\n', &src[ls as usize..]).map(|k| ls as usize + k).unwrap_or(src.len());
        let line_bytes = &src[ls as usize..le];
        if (me as usize) > le || ms < ls {
            if std::env::var_os("GREEG_DEBUG").is_some() {
                eprintln!("greeg: offset mismatch in {} line {} ls={} le={} ms={} me={}", rel, line, ls, le, ms, me);
            }
            continue;
        }
        let kind = if cx.classify && !flags.has(FileFlags::MINIFIED) { classify_line(lang, line_bytes, (ms - ls) as usize, (me - ls) as usize) } else { HitKind::Ident };
        if cx.filter_kinds && !o.kinds.is_empty() && !o.kinds.contains(&kind) {
            continue;
        }
        kinds[kind.idx()] += 1;
        let exact = !o.fixed_strings && !o.case_insensitive && &src[ms as usize..me as usize] == pat && (ms == 0 || !is_word_byte(src[ms as usize - 1])) && (me as usize >= src.len() || !is_word_byte(src[me as usize]));
        let score = kind.weight() * prior * if exact { 1.15 } else { 1.0 };
        let lead = line_bytes.len() - greeg_lang::trim_start(line_bytes).len().min(line_bytes.len());
        let (text, clipped, tm) = clip_line(&line_bytes[lead..], ((ms - ls) as usize).saturating_sub(lead), ((me - ls) as usize).saturating_sub(lead), o.max_columns);
        hits.push(Hit { line: line as u32, line_start: ls, match_start: ms, match_end: me, kind, chain: Vec::new(), def_idx: None, score, text, text_match: (tm.0 as u32, tm.1 as u32), clipped });
    }
    cx.stats.classify_ns.fetch_add(t_cls.elapsed().as_nanos() as u64, Relaxed);
    if hits.is_empty() {
        return None;
    }
    Some(FileResult { rel, path: path.to_path_buf(), lang, flags, size, age_days, prior, hits, total: sink.total, kinds, defs: Vec::new(), refined: false, file_id: None })
}

/// Clip a line around the match. Returns the text, whether it was clipped,
/// and the match range within the returned text.
pub fn clip_line(line: &[u8], ms: usize, me: usize, max_cols: usize) -> (Vec<u8>, bool, (usize, usize)) {
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

/// Full outline for one file: refine hit kinds, attach enclosing chains, fill `defs`.
pub fn refine_file(f: &mut FileResult) {
    if f.refined || !f.lang.has_grammar() || f.flags.has(FileFlags::MINIFIED) {
        return;
    }
    let t_read = Instant::now();
    let Ok(full) = greeg_lang::read_text(&f.path) else { return };
    let read_us = t_read.elapsed().as_micros();
    // Lex and outline a window around the hits, not the whole file: a hit
    // deep inside a 2.5 MB file must not cost 10 ms of parsing. The window
    // starts at a line boundary up to 512 KiB before the first hit (lexer
    // state is assumed "code" there; a block comment spanning that point is
    // the accepted inaccuracy) and ends 64 KiB after the last hit.
    const BEFORE: usize = 512 * 1024;
    const AFTER: usize = 64 * 1024;
    let first = f.hits.iter().map(|h| h.line_start as usize).min().unwrap_or(0);
    let last = f.hits.iter().map(|h| h.match_end as usize).max().unwrap_or(0);
    let start = if first > BEFORE { let s = first - BEFORE; memchr::memchr(b'\n', &full[s..]).map(|k| s + k + 1).unwrap_or(s) } else { 0 };
    let cut = (last + AFTER).min(full.len());
    let cut = if cut < full.len() { memchr::memchr(b'\n', &full[cut..]).map(|k| cut + k + 1).unwrap_or(full.len()) } else { full.len() };
    let window = &full[start..cut];
    let t_lex = Instant::now();
    let lexed = lex(f.lang, window);
    let lex_us = t_lex.elapsed().as_micros();
    let t_ol = Instant::now();
    let mut ol = outline(f.lang, window, &lexed);
    if std::env::var_os("GREEG_DEBUG").is_some() {
        eprintln!("refine {} full={} window={} read={}us lex={}us outline={}us defs={}", f.rel, full.len(), window.len(), read_us, lex_us, t_ol.elapsed().as_micros(), ol.defs.len());
    }
    // shift window-relative offsets to absolute
    let base = start as u32;
    for d in &mut ol.defs {
        d.start += base;
        d.end += base;
        d.name_start += base;
        d.name_end += base;
        d.line += 0; // line numbers are recomputed below from the window
    }
    let line_base = memchr::memchr_iter(b'\n', &full[..start]).count() as u32;
    for d in &mut ol.defs {
        d.line += line_base;
    }
    let src = &full[..];
    let lexed = shift_lexed(lexed, base);
    for h in &mut f.hits {
        if (h.match_end as usize) > src.len() {
            continue;
        }
        let ls = h.line_start as usize;
        let le = memchr::memchr(b'\n', &src[ls..]).map(|k| ls + k).unwrap_or(src.len());
        let (kind, di) = classify_full(f.lang, src, &lexed, &ol, h.match_start, h.match_end, h.line_start, &src[ls..le]);
        h.kind = kind;
        h.def_idx = di;
        h.chain = di.map(|d| ol.chain(d, src)).unwrap_or_default();
    }
    f.defs = ol.defs.iter().enumerate().map(|(i, d)| DefSummary { name: String::from_utf8_lossy(&src[d.name_start as usize..d.name_end as usize]).into_owned(), kind: d.kind, line: d.line, start: d.start, end: d.end, chain: ol.chain(i as u32, src), flags: 0 }).collect();
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
    let mut taken: Vec<(usize, FileResult)> = idx.iter().map(|&i| (i, std::mem::replace(&mut r.files[i], FileResult { rel: String::new(), path: PathBuf::new(), lang: Lang::None, flags: FileFlags::default(), size: 0, age_days: 0.0, prior: 0.0, hits: vec![], total: 0, kinds: [0; 9], defs: vec![], refined: false, file_id: None }))).collect();
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
    pub(crate) classify_ns: std::sync::atomic::AtomicU64,
    pub(crate) read_ns: std::sync::atomic::AtomicU64,
    pub(crate) search_ns: std::sync::atomic::AtomicU64,
}

fn scan_once(o: &Options) -> Result<ScanResult> {
    let t0 = Instant::now();
    let matcher = build_matcher(o)?;
    let threads = if o.threads == 0 { default_threads() } else { o.threads };
    let acc = StatsAcc::default();
    let classify = matches!(o.mode, Mode::Content | Mode::Outline | Mode::Block) || !o.kinds.is_empty();
    if classify {
        // compile the definition regexes while the walk starts
        std::thread::spawn(greeg_lang::defs::warm);
    }
    let cx = Ctx { o, matcher: &matcher, stats: &acc, classify, filter_kinds: true };
    if o.use_index && !o.no_ignore && !o.hidden {
        // A panic anywhere in the index path degrades to scan mode (DESIGN.md §12):
        // the answer is still correct, one line goes to stderr, and the index is rebuilt.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| indexed::try_index(&cx, threads, t0))) {
            Ok(Ok(Some(r))) => return Ok(r),
            Ok(Ok(None)) => {}
            Ok(Err(e)) => {
                if std::env::var_os("GREEG_DEBUG").is_some() {
                    eprintln!("greeg: index unavailable: {e:#}");
                }
            }
            Err(_) => {
                eprintln!("greeg: internal error in the index path; answering from a scan and rebuilding the index");
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
    walker(o, threads)?.run(|| {
        let mut sb = SearcherBuilder::new();
        // bom_sniffing(false): offsets must index the buffer we read (the
        // searcher would otherwise strip a UTF-8 BOM and shift every offset by 3).
        // UTF-16 files are therefore searched as raw bytes (ripgrep transcodes them).
        sb.line_number(true).binary_detection(BinaryDetection::quit(0)).multi_line(o.multiline).bom_sniffing(false);
        let mut searcher = sb.build();
        let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
        let mut local = Local { v: Vec::new(), out: &out };
        let cx = &cx;
        Box::new(move |entry| {
            let Ok(e) = entry else { return ignore::WalkState::Continue };
            if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                return ignore::WalkState::Continue;
            }
            let p = e.path();
            cx.stats.walked.fetch_add(1, Relaxed);
            let rel = p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/");
            if let Some(fr) = process_file(cx, p, rel, &mut searcher, &mut buf) {
                local.v.push(fr);
                if local.v.len() >= 64 {
                    local.out.lock().unwrap().append(&mut local.v);
                }
            }
            ignore::WalkState::Continue
        })
    });
    let mut files = out.into_inner().unwrap();
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    let mut stats = Stats { files_walked: acc.walked.load(Relaxed), files_searched: acc.searched.load(Relaxed), skipped_binary: acc.binary.load(Relaxed), skipped_huge: acc.huge.load(Relaxed), threads, ..Default::default() };
    for f in &files {
        stats.files_matched += 1;
        stats.total_hits += f.total;
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
    stats.elapsed_ms = t0.elapsed().as_secs_f64() * 1e3;
    stats.cpu_read_ms = acc.read_ns.load(Relaxed) as f64 / 1e6;
    stats.cpu_search_ms = acc.search_ns.load(Relaxed) as f64 / 1e6;
    stats.cpu_classify_ms = acc.classify_ns.load(Relaxed) as f64 / 1e6;
    stats.source = "scan";
    Ok(ScanResult { opts: o.clone(), files, stats, rung: Rung::Exact, ignored_only: None })
}

/// Finish a `Stats` from per-file results (shared by scan and index paths).
pub(crate) fn finish_stats(stats: &mut Stats, files: &[FileResult], o: &Options, acc: &StatsAcc, t0: Instant) {
    for f in files {
        stats.files_matched += 1;
        stats.total_hits += f.total;
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

/// Scan with the escalation ladder (rungs 1–5, DESIGN.md §7.6).
pub fn scan(o: &Options) -> Result<ScanResult> {
    let mut r = scan_once(o)?;
    if r.stats.total_hits > 0 || !o.ladder {
        return Ok(r);
    }
    let mut elapsed = r.stats.elapsed_ms;
    if o.word {
        let mut o2 = o.clone();
        o2.word = false;
        let mut r2 = scan_once(&o2)?;
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
        let mut r2 = scan_once(&o2)?;
        elapsed += r2.stats.elapsed_ms;
        if r2.stats.total_hits > 0 {
            r2.rung = Rung::CaseInsensitive;
            r2.stats.elapsed_ms = elapsed;
            return Ok(r2);
        }
        r = r2;
    }
    // rungs 3 and 4 need the symbol names (DESIGN.md §7.6)
    if o.use_index && !o.no_ignore && !o.hidden && (o.fixed_strings || is_plain_word(&o.pattern)) {
        let threads = if o.threads == 0 { default_threads() } else { o.threads };
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
                alts = op.idx.fuzzy_names(&o.pattern, d, 6).into_iter().map(|(n, _)| n).collect();
                if !alts.is_empty() {
                    rung = Rung::Fuzzy(alts.clone());
                }
            }
            drop(op);
            if !alts.is_empty() {
                let mut o2 = o.clone();
                o2.pattern = alts.iter().map(|a| regex_escape(a)).collect::<Vec<_>>().join("|");
                o2.fixed_strings = false;
                o2.word = true;
                o2.case_insensitive = false;
                let mut r2 = scan_once(&o2)?;
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
    if !o.no_ignore || !o.hidden {
        let mut o2 = o.clone();
        o2.no_ignore = true;
        o2.hidden = true;
        o2.mode = Mode::Count;
        o2.budget = 0;
        let r2 = scan_once(&o2)?;
        elapsed += r2.stats.elapsed_ms;
        if r2.stats.total_hits > 0 {
            r.ignored_only = Some((r2.stats.files_matched, r2.stats.total_hits));
            r.rung = Rung::Ignored;
        }
    }
    r.stats.elapsed_ms = elapsed;
    Ok(r)
}
