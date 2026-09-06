//! Stage-B extraction (DESIGN.md §7.1): tree-sitter parse, one tags query
//! per language, and a small amount of post-processing into symbols, noncode
//! spans and import statements. Falls back to the regex outline (§7.5) on
//! parse timeout or when `ERROR` nodes cover more than 20 % of the file.

use crate::lexer::{Span, SpanKind};
use crate::{DefKind, Lang, defs, lexer};
use std::cell::RefCell;
use std::ops::ControlFlow;
use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Node, ParseOptions, Parser, Query, QueryCursor};

/// Symbol flags (stored as `u8` in the index).
pub const SYM_EXPORTED: u8 = 1;
pub const SYM_HAS_DOC: u8 = 2;
pub const SYM_TEST: u8 = 4;
/// JS/TS: a member of an object literal (`{ watchFile: () => … }`, `{ get(k) {…} }`).
/// Usually an implementation of a typed member rather than the definition an
/// agent asks for: hits on its name classify as `member`, and `def` ranks it low.
pub const SYM_OBJ_MEMBER: u8 = 8;

pub const PARSE_TIMEOUT: Duration = Duration::from_millis(200);
/// Files whose `ERROR` nodes cover more than this fraction fall back to regexes.
pub const MAX_ERROR_RATIO: f32 = 0.20;

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name_start: u32,
    pub name_end: u32,
    /// Definition node range (start of the declaration, end of the body).
    pub start: u32,
    pub end: u32,
    /// 1-based line of `start`.
    pub line: u32,
    pub kind: DefKind,
    pub parent: Option<u32>,
    pub flags: u8,
    /// Supertype name ranges (bytes into the source).
    pub supers: Vec<(u32, u32)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    pub start: u32,
    pub end: u32,
    /// Module path as written, normalized per language: `a.b.c` (Python,
    /// Kotlin), `./x` (JS/TS), `crate::a::b` (Rust, one entry per leaf).
    pub module: String,
    /// Imported names (empty for whole-module imports).
    pub names: Vec<String>,
    pub wildcard: bool,
}

#[derive(Debug, Default)]
pub struct Extract {
    pub symbols: Vec<Symbol>,
    pub noncode: Vec<Span>,
    pub imports: Vec<Import>,
    /// Kotlin `package a.b`.
    pub package: Option<String>,
    /// Any `ERROR`/`MISSING` node in the parse.
    pub parse_errors: bool,
    /// False when the regex extractor produced this result.
    pub tree_sitter: bool,
}

impl Extract {
    pub fn name<'a>(&self, s: &Symbol, src: &'a [u8]) -> &'a str {
        std::str::from_utf8(&src[s.name_start as usize..s.name_end as usize]).unwrap_or("")
    }
    /// Innermost symbol whose range contains `off`.
    pub fn enclosing(&self, off: u32) -> Option<u32> {
        let upto = self.symbols.partition_point(|d| d.start <= off);
        (0..upto)
            .rev()
            .find(|&i| self.symbols[i].end > off)
            .map(|i| i as u32)
    }
}

#[derive(Clone, Copy, Debug)]
enum Cap {
    Def(DefKind),
    Name,
    Supers,
    Import,
    Package,
    Noncode(SpanKind),
    /// Rust: the `{ … }` token tree of a macro invocation (`cfg_rt! { … }`), re-parsed as items.
    MacroBody,
    Ignore,
}

struct LangQ {
    lang: Language,
    query: Query,
    caps: Vec<Cap>,
}

fn kind_of(s: &str) -> DefKind {
    match s {
        "function" => DefKind::Function,
        "method" => DefKind::Method,
        "class" => DefKind::Class,
        "struct" => DefKind::Struct,
        "enum" => DefKind::Enum,
        "trait" => DefKind::Trait,
        "interface" => DefKind::Interface,
        "typealias" => DefKind::TypeAlias,
        "module" => DefKind::Module,
        "object" => DefKind::Object,
        "impl" => DefKind::Impl,
        "constant" => DefKind::Constant,
        "variable" => DefKind::Variable,
        "macro" => DefKind::Macro,
        "field" => DefKind::Field,
        "variant" => DefKind::Variant,
        "property" => DefKind::Field,
        "constructor" => DefKind::Method,
        _ => DefKind::Variable,
    }
}

fn cap_of(n: &str) -> Cap {
    match n {
        "name" => Cap::Name,
        "supers" => Cap::Supers,
        "import" => Cap::Import,
        "package" => Cap::Package,
        "macro_body" => Cap::MacroBody,
        "noncode.comment" => Cap::Noncode(SpanKind::Comment),
        "noncode.string" => Cap::Noncode(SpanKind::String),
        "noncode.docstring" => Cap::Noncode(SpanKind::Docstring),
        n if n.starts_with("def.") => Cap::Def(kind_of(&n[4..])),
        _ => Cap::Ignore,
    }
}

fn compile(lang: Language, src: &str) -> LangQ {
    let query = Query::new(&lang, src).unwrap_or_else(|e| panic!("bad query: {e}"));
    let caps = query.capture_names().iter().map(|n| cap_of(n)).collect();
    LangQ { lang, query, caps }
}

static PY: LazyLock<LangQ> = LazyLock::new(|| {
    compile(
        tree_sitter_python::LANGUAGE.into(),
        include_str!("../queries/python.scm"),
    )
});
static RS: LazyLock<LangQ> = LazyLock::new(|| {
    compile(
        tree_sitter_rust::LANGUAGE.into(),
        include_str!("../queries/rust.scm"),
    )
});
static JS: LazyLock<LangQ> = LazyLock::new(|| {
    compile(
        tree_sitter_javascript::LANGUAGE.into(),
        include_str!("../queries/javascript.scm"),
    )
});
static TS: LazyLock<LangQ> = LazyLock::new(|| {
    compile(
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        include_str!("../queries/typescript.scm"),
    )
});
static TSX: LazyLock<LangQ> = LazyLock::new(|| {
    compile(
        tree_sitter_typescript::LANGUAGE_TSX.into(),
        include_str!("../queries/typescript.scm"),
    )
});
static KT: LazyLock<LangQ> = LazyLock::new(|| {
    compile(
        tree_sitter_kotlin_sg::LANGUAGE.into(),
        include_str!("../queries/kotlin.scm"),
    )
});

fn lang_q(lang: Lang, tsx: bool) -> Option<&'static LangQ> {
    Some(match lang {
        Lang::Python => &PY,
        Lang::Rust => &RS,
        Lang::JavaScript => &JS,
        Lang::TypeScript if tsx => &TSX,
        Lang::TypeScript => &TS,
        Lang::Kotlin => &KT,
        Lang::Extra(i) => return extra_q(i),
        _ => return None,
    })
}

/// Runtime-loaded grammars: `dlopen` + query compile on first use, cached per language.
fn extra_q(i: u8) -> Option<&'static LangQ> {
    static CACHE: OnceLock<Vec<OnceLock<Option<LangQ>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        (0..crate::extra::registry().len())
            .map(|_| OnceLock::new())
            .collect()
    });
    cache
        .get(i as usize)?
        .get_or_init(|| {
            let (lang, query) = crate::extra::grammar(i)?;
            match Query::new(&lang, &query) {
                Ok(q) => {
                    let caps = q.capture_names().iter().map(|n| cap_of(n)).collect();
                    Some(LangQ {
                        lang,
                        query: q,
                        caps,
                    })
                }
                Err(e) => {
                    eprintln!(
                        "greeg: extra language {}: bad tags.scm: {e}",
                        crate::extra::get(i).map(|l| l.name).unwrap_or("?")
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// Is the runtime grammar for an extra language usable? (`greeg doctor`, `greeg lang check`)
pub fn extra_ready(i: u8) -> bool {
    extra_q(i).is_some()
}

/// Compile all queries (tens of ms in total); call on a helper thread.
pub fn warm() {
    let _ = (&*PY, &*RS, &*JS, &*TS, &*TSX, &*KT);
}

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(Parser::new());
    static CURSOR: RefCell<QueryCursor> = RefCell::new(QueryCursor::new());
}

/// Does the file name select the TSX parser?
pub fn is_tsx(rel: &str) -> bool {
    rel.ends_with(".tsx")
}

/// Extract symbols, noncode spans and imports. `tsx` selects the TSX parser
/// for TypeScript. Always returns something: the regex extractor is the
/// fallback for timeouts, heavy parse errors and languages without a grammar.
pub fn extract(lang: Lang, tsx: bool, src: &[u8]) -> Extract {
    let mut ex = extract_raw(lang, tsx, src, 0);
    if ex.tree_sitter {
        finish(&mut ex, lang, src);
    }
    ex
}

/// Rust macro bodies worth re-parsing: brace-delimited and containing an item keyword.
fn macro_body_has_items(t: &[u8]) -> bool {
    if t.first() != Some(&b'{') || t.len() < 8 {
        return false;
    }
    [
        &b"fn "[..],
        b"struct ",
        b"impl",
        b"enum ",
        b"trait ",
        b"mod ",
        b"const ",
        b"static ",
        b"type ",
        b"macro_rules!",
    ]
    .iter()
    .any(|k| memchr::memmem::find(t, k).is_some())
}

fn extract_raw(lang: Lang, tsx: bool, src: &[u8], depth: u8) -> Extract {
    let Some(lq) = lang_q(lang, tsx) else {
        return regex_extract(lang, src);
    };
    let tree = PARSER.with(|p| {
        let mut p = p.borrow_mut();
        if p.set_language(&lq.lang).is_err() {
            return None;
        }
        let t0 = Instant::now();
        let mut cb = |_: &tree_sitter::ParseState| {
            if t0.elapsed() > PARSE_TIMEOUT {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let opts = ParseOptions::new().progress_callback(&mut cb);
        p.parse_with_options(
            &mut |off, _| if off < src.len() { &src[off..] } else { &[] },
            None,
            Some(opts),
        )
    });
    let Some(tree) = tree else {
        return regex_extract(lang, src);
    };
    let root = tree.root_node();
    let mut parse_errors = false;
    if root.has_error() {
        parse_errors = true;
        let bad = error_bytes(root);
        if bad as f32 > src.len() as f32 * MAX_ERROR_RATIO {
            let mut e = regex_extract(lang, src);
            e.parse_errors = true;
            return e;
        }
    }
    let mut ex = Extract {
        parse_errors,
        tree_sitter: true,
        ..Default::default()
    };
    let mut macro_bodies: Vec<(u32, u32)> = Vec::new();
    CURSOR.with(|c| {
        let mut cursor = c.borrow_mut();
        let mut it = cursor.matches(&lq.query, root, src);
        while let Some(m) = it.next() {
            let mut def: Option<(DefKind, Node)> = None;
            let mut name: Option<Node> = None;
            let mut supers: Vec<(u32, u32)> = Vec::new();
            for c in m.captures() {
                match lq.caps[c.index as usize] {
                    Cap::Def(k) => def = Some((k, c.node)),
                    Cap::Name => name = Some(c.node),
                    Cap::Supers => super_names(
                        &src[c.node.byte_range()],
                        c.node.start_byte() as u32,
                        &mut supers,
                    ),
                    Cap::Import => {
                        let text = &src[c.node.byte_range()];
                        parse_imports(
                            lang,
                            text,
                            c.node.start_byte() as u32,
                            c.node.end_byte() as u32,
                            &mut ex.imports,
                        );
                    }
                    Cap::Package => {
                        let t = String::from_utf8_lossy(&src[c.node.byte_range()]);
                        let t = t
                            .trim()
                            .trim_start_matches("package")
                            .trim()
                            .trim_end_matches(';')
                            .trim();
                        if !t.is_empty() {
                            ex.package = Some(t.to_string());
                        }
                    }
                    Cap::MacroBody => {
                        if depth < 2 && macro_body_has_items(&src[c.node.byte_range()]) {
                            macro_bodies
                                .push((c.node.start_byte() as u32, c.node.end_byte() as u32));
                        }
                    }
                    Cap::Noncode(k) => {
                        let (s, e) = (c.node.start_byte() as u32, c.node.end_byte() as u32);
                        let k = if k == SpanKind::Comment
                            && is_doc_comment(lang, &src[s as usize..e as usize])
                        {
                            SpanKind::Docstring
                        } else {
                            k
                        };
                        ex.noncode.push(Span {
                            start: s,
                            end: e,
                            kind: k,
                        });
                    }
                    Cap::Ignore => {}
                }
            }
            if let Some((mut kind, node)) = def {
                if kind == DefKind::Impl
                    && let Some(n) = name
                {
                    // `impl Trait for (A, B)` / `impl Trait for [T; N]`: no identifier to narrow to.
                    // Name the block by the trait if there is one, else by the type text.
                    let (ns, ne) = narrow_name(src, n.start_byte() as u32, n.end_byte() as u32);
                    if ne <= ns {
                        if let Some(&(a, b)) = supers.first() {
                            kind = DefKind::Impl;
                            ex.symbols.push(Symbol {
                                name_start: a,
                                name_end: b,
                                start: node.start_byte() as u32,
                                end: node.end_byte() as u32,
                                line: node.start_position().row as u32 + 1,
                                kind,
                                parent: None,
                                flags: 0,
                                supers: std::mem::take(&mut supers),
                            });
                        } else {
                            ex.symbols.push(Symbol {
                                name_start: n.start_byte() as u32,
                                name_end: n.end_byte() as u32,
                                start: node.start_byte() as u32,
                                end: node.end_byte() as u32,
                                line: node.start_position().row as u32 + 1,
                                kind,
                                parent: None,
                                flags: 0,
                                supers: Vec::new(),
                            });
                        }
                        continue;
                    }
                }
                if lang == Lang::Kotlin {
                    if kind == DefKind::Class {
                        kind = kotlin_class_kind(node, src);
                    }
                    if node.kind() == "companion_object" && name.is_none() {
                        name = (0..node.child_count())
                            .filter_map(|i| node.child(i))
                            .find(|ch| ch.kind() == "type_identifier");
                    }
                }
                let (ns, ne) = match name {
                    Some(n) => narrow_name(src, n.start_byte() as u32, n.end_byte() as u32),
                    None => keyword_name(src, node, kind),
                };
                if ne <= ns {
                    continue;
                }
                let name_bytes = &src[ns as usize..ne as usize];
                if lang == Lang::Python && kind == DefKind::Variable && is_const_name(name_bytes) {
                    kind = DefKind::Constant;
                }
                let mut flags = 0u8;
                if exported(lang, node, name, src, name_bytes) {
                    flags |= SYM_EXPORTED;
                }
                if is_test(lang, node, src, name_bytes) {
                    flags |= SYM_TEST;
                }
                if matches!(lang, Lang::JavaScript | Lang::TypeScript)
                    && node.parent().map(|p| p.kind() == "object").unwrap_or(false)
                {
                    flags |= SYM_OBJ_MEMBER;
                }
                // line = the name's line (annotations, decorators and modifiers may precede the
                // declaration on earlier lines; `start` still covers them for block extraction)
                let line = node.start_position().row as u32
                    + 1
                    + memchr::memchr_iter(
                        b'\n',
                        &src[node.start_byte()..(ns as usize).max(node.start_byte())],
                    )
                    .count() as u32;
                ex.symbols.push(Symbol {
                    name_start: ns,
                    name_end: ne,
                    start: node.start_byte() as u32,
                    end: node.end_byte() as u32,
                    line,
                    kind,
                    parent: None,
                    flags,
                    supers: std::mem::take(&mut supers),
                });
            }
        }
    });
    // Rust: items hidden inside `cfg_rt! { … }`-style macro bodies (tokio, hyper, …)
    for (a, b) in macro_bodies {
        let (ia, ib) = (a as usize + 1, b as usize - 1);
        if ib <= ia {
            continue;
        }
        let sub = extract_raw(lang, tsx, &src[ia..ib], depth + 1);
        if !sub.tree_sitter {
            continue;
        }
        let base = ia as u32;
        let base_line = memchr::memchr_iter(b'\n', &src[..ia]).count() as u32;
        for mut s in sub.symbols {
            s.name_start += base;
            s.name_end += base;
            s.start += base;
            s.end += base;
            s.line += base_line;
            s.parent = None;
            for sp in &mut s.supers {
                sp.0 += base;
                sp.1 += base;
            }
            ex.symbols.push(s);
        }
        for mut n in sub.noncode {
            n.start += base;
            n.end += base;
            ex.noncode.push(n);
        }
        for mut i in sub.imports {
            i.start += base;
            i.end += base;
            ex.imports.push(i);
        }
    }
    ex
}

/// Bytes covered by ERROR/MISSING nodes (walks only into subtrees with errors).
fn error_bytes(root: Node) -> usize {
    let mut total = 0;
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.is_error() || n.is_missing() {
            total += n.byte_range().len();
            continue;
        }
        if !n.has_error() {
            continue;
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    total
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// `a::b::Foo<T>` → `Foo`; `Foo<T>` → `Foo`; `this.x` → `x`; `` `weird name` `` kept.
fn narrow_name(src: &[u8], s: u32, e: u32) -> (u32, u32) {
    let t = &src[s as usize..e as usize];
    if t.first() == Some(&b'`') {
        return (s, e);
    }
    // quoted module names: TS `declare module 'x'` → `x`
    if t.len() >= 2 && (t[0] == b'\'' || t[0] == b'"') && t[t.len() - 1] == t[0] {
        return (s + 1, e - 1);
    }
    let cut = t
        .iter()
        .position(|&b| b == b'<' || b == b'(' || b == b'[')
        .unwrap_or(t.len());
    let head = &t[..cut];
    let last = head
        .iter()
        .rposition(|&b| !is_word(b))
        .map(|p| p + 1)
        .unwrap_or(0);
    let mut end = cut;
    while end > last && !is_word(head[end - 1]) {
        end -= 1;
    }
    (s + last as u32, s + end as u32)
}

/// Name range for definitions without a name node (Kotlin `companion object`,
/// secondary constructors): the keyword itself.
fn keyword_name(src: &[u8], node: Node, kind: DefKind) -> (u32, u32) {
    let t = &src[node.byte_range()];
    let kw: &[u8] = match kind {
        DefKind::Object => b"companion",
        _ => b"constructor",
    };
    match memchr::memmem::find(t, kw) {
        Some(p) => (
            node.start_byte() as u32 + p as u32,
            node.start_byte() as u32 + (p + kw.len()) as u32,
        ),
        None => (node.start_byte() as u32, node.start_byte() as u32),
    }
}

fn is_doc_comment(lang: Lang, t: &[u8]) -> bool {
    match lang {
        Lang::Rust => {
            t.starts_with(b"///")
                || t.starts_with(b"//!")
                || t.starts_with(b"/**")
                || t.starts_with(b"/*!")
        }
        Lang::JavaScript | Lang::TypeScript | Lang::Kotlin => {
            t.starts_with(b"/**") && !t.starts_with(b"/**/")
        }
        _ => false,
    }
}

/// Split a supertype list (`Base, metaclass=M`, `extends A<T> implements B`,
/// `: Send + Sync`, `Foo(), Bar by x`) into last-segment identifier ranges.
fn super_names(t: &[u8], base: u32, out: &mut Vec<(u32, u32)>) {
    let (t, base) = if t.first() == Some(&b'(') && t.last() == Some(&b')') {
        (&t[1..t.len() - 1], base + 1)
    } else {
        (t, base)
    };
    const SEP_WORDS: &[&[u8]] = &[b"extends", b"implements", b"by", b"where", b"with"];
    let mut depth = 0i32;
    let mut i = 0;
    let mut elem_start = 0;
    let flush = |a: usize, b: usize, out: &mut Vec<(u32, u32)>| {
        let e = &t[a..b];
        if e.contains(&b'=') || e.contains(&b'\'') {
            return;
        }
        // leading path
        let mut j = 0;
        while j < e.len()
            && (e[j] == b' '
                || e[j] == b'\t'
                || e[j] == b'\n'
                || e[j] == b'\r'
                || e[j] == b':'
                || e[j] == b'('
                || e[j] == b'?')
        {
            j += 1;
        }
        let ps = j;
        while j < e.len() && (is_word(e[j]) || e[j] == b'.' || e[j] == b':') {
            j += 1;
        }
        let path = &e[ps..j];
        if path.is_empty() {
            return;
        }
        let last = path
            .iter()
            .rposition(|&b| !is_word(b))
            .map(|p| p + 1)
            .unwrap_or(0);
        if last >= path.len() {
            return;
        }
        let name = &path[last..];
        if SEP_WORDS.contains(&name) || name == b"object" {
            return;
        }
        out.push((
            base + (a + ps + last) as u32,
            base + (a + ps + path.len()) as u32,
        ));
    };
    while i < t.len() {
        match t[i] {
            b'(' | b'<' | b'[' | b'{' => depth += 1,
            b')' | b'>' | b']' | b'}' => depth -= 1,
            b',' | b'+' | b'&' if depth <= 0 => {
                flush(elem_start, i, out);
                elem_start = i + 1;
            }
            b if depth <= 0 && is_word(b) && (i == 0 || !is_word(t[i - 1])) => {
                let mut j = i;
                while j < t.len() && is_word(t[j]) {
                    j += 1;
                }
                if SEP_WORDS.contains(&&t[i..j]) {
                    flush(elem_start, i, out);
                    elem_start = j;
                    i = j;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    flush(elem_start, t.len(), out);
}

/// Python module-level constant: `MAX`, `_CACHE_SIZE` (`[A-Z_][A-Z0-9_]{2,}`,
/// the same rule as the regex extractor).
fn is_const_name(name: &[u8]) -> bool {
    name.len() >= 3
        && (name[0].is_ascii_uppercase() || name[0] == b'_')
        && name
            .iter()
            .all(|&b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// Python visibility from the name: `_private` is not exported, dunder names
/// (`__init__`, `__all__`) are.
pub fn python_name_exported(name: &[u8]) -> bool {
    !name.starts_with(b"_") || (name.starts_with(b"__") && name.ends_with(b"__") && name.len() > 4)
}

/// Kotlin `class_declaration` kind from its keyword children: `interface`
/// (also `fun interface`, `sealed interface`) and `enum class`. Annotation
/// text is never consulted.
fn kotlin_class_kind(node: Node, src: &[u8]) -> DefKind {
    for i in 0..node.child_count() {
        let Some(ch) = node.child(i) else { break };
        if ch.kind() == "type_identifier" {
            break;
        }
        if !ch.is_named() {
            match ch.kind() {
                "interface" => return DefKind::Interface,
                "enum" => return DefKind::Enum,
                _ => {}
            }
        } else if ch.kind() == "modifiers" {
            // older grammars spell `enum` as a class_modifier
            for j in 0..ch.child_count() {
                let Some(m) = ch.child(j) else { break };
                if m.kind() == "class_modifier" && &src[m.byte_range()] == b"enum" {
                    return DefKind::Enum;
                }
            }
        }
    }
    DefKind::Class
}

/// Kotlin: does the declaration's `modifiers` node carry `private` or `internal`?
fn kotlin_hidden(node: Node, src: &[u8]) -> bool {
    let Some(mods) = node.child(0).filter(|c| c.kind() == "modifiers") else {
        return false;
    };
    for i in 0..mods.child_count() {
        let Some(m) = mods.child(i) else { break };
        if m.kind() == "visibility_modifier"
            && matches!(&src[m.byte_range()], b"private" | b"internal")
        {
            return true;
        }
    }
    false
}

fn exported(lang: Lang, node: Node, name: Option<Node>, src: &[u8], name_bytes: &[u8]) -> bool {
    match lang {
        Lang::Rust => {
            // plain `pub` only: `pub(crate)`, `pub(super)`, `pub(in path)` have a named child
            (0..2)
                .filter_map(|i| node.child(i))
                .any(|ch| ch.kind() == "visibility_modifier" && ch.named_child_count() == 0)
        }
        Lang::Python => python_name_exported(name_bytes),
        Lang::JavaScript | Lang::TypeScript => {
            if name
                .map(|n| n.kind() == "private_property_identifier")
                .unwrap_or(false)
            {
                return false;
            }
            // declarator → declaration → (ambient_declaration) → export_statement;
            // never through a function or class body (a nested declaration is not
            // exported because its enclosing function is)
            let mut p = Some(node);
            for _ in 0..4 {
                match p {
                    Some(n) if n.kind() == "export_statement" => return true,
                    Some(n)
                        if n.id() == node.id()
                            || matches!(
                                n.kind(),
                                "variable_declarator"
                                    | "lexical_declaration"
                                    | "variable_declaration"
                                    | "ambient_declaration"
                            ) =>
                    {
                        p = n.parent()
                    }
                    _ => break,
                }
            }
            matches!(
                node.kind(),
                "method_definition"
                    | "method_signature"
                    | "abstract_method_signature"
                    | "public_field_definition"
                    | "field_definition"
                    | "property_signature"
            )
        }
        Lang::Kotlin => !kotlin_hidden(node, src),
        _ => false,
    }
}

/// Rust: is `attr` (an `attribute_item`) a test attribute for `item`?
/// `#[test]`, `#[<path>::test]`, `#[rstest…]`, `#[tokio::test…]`,
/// `#[async_std::test]`, `#[wasm_bindgen_test]`, and `#[cfg(test)]` on a
/// `mod`. `#[cfg(not(test))]` and `#[cfg_attr(test, …)]` are not.
fn rust_test_attribute(attr: Node, item: Node, src: &[u8]) -> bool {
    let Some(a) = attr.named_child(0).filter(|a| a.kind() == "attribute") else {
        return false;
    };
    let Some(path) = a.named_child(0) else {
        return false;
    };
    let path = &src[path.byte_range()];
    let last = path.rsplit(|&b| b == b':').next().unwrap_or(path);
    match last {
        b"test" => true,
        b"wasm_bindgen_test" => true,
        _ if last.starts_with(b"rstest") => true,
        b"cfg" if item.kind() == "mod_item" => {
            let args = a
                .child_by_field_name("arguments")
                .map(|n| &src[n.byte_range()])
                .unwrap_or(b"");
            args.trim_ascii() == b"(test)"
        }
        _ => false,
    }
}

fn is_test(lang: Lang, node: Node, src: &[u8], name: &[u8]) -> bool {
    match lang {
        Lang::Python => name.starts_with(b"test_") || name.starts_with(b"Test"),
        Lang::Rust => {
            if name.starts_with(b"test") {
                return true;
            }
            // the attributes are the previous siblings
            let mut prev = node.prev_named_sibling();
            while let Some(p) = prev {
                if p.kind() != "attribute_item" {
                    return false;
                }
                if rust_test_attribute(p, node, src) {
                    return true;
                }
                prev = p.prev_named_sibling();
            }
            false
        }
        Lang::Kotlin => name.starts_with(b"test") || name.starts_with(b"`"),
        Lang::JavaScript | Lang::TypeScript => name.starts_with(b"test"),
        _ => false,
    }
}

/// Post-processing shared by both extractors: ordering, dedup, parent links,
/// method/field normalization, doc flags, and Python/Kotlin visibility fixes.
fn finish(ex: &mut Extract, lang: Lang, src: &[u8]) {
    ex.symbols.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then(b.end.cmp(&a.end))
            .then(a.name_start.cmp(&b.name_start))
    });
    ex.symbols.dedup_by(|b, a| {
        if a.start == b.start && a.name_start == b.name_start {
            if a.kind == DefKind::Variable && b.kind != DefKind::Variable {
                a.kind = b.kind;
            }
            // quantified `@supers` captures arrive as one match per element
            for sp in b.supers.drain(..) {
                if !a.supers.contains(&sp) {
                    a.supers.push(sp);
                }
            }
            true
        } else {
            false
        }
    });
    // parents by containment (stack of open ranges)
    let mut stack: Vec<u32> = Vec::new();
    for i in 0..ex.symbols.len() {
        let (s, e) = (ex.symbols[i].start, ex.symbols[i].end);
        while let Some(&top) = stack.last() {
            if ex.symbols[top as usize].end <= s {
                stack.pop();
            } else {
                break;
            }
        }
        if let Some(&top) = stack.last() {
            ex.symbols[i].parent = Some(top);
            let pk = ex.symbols[top as usize].kind;
            let pflags = ex.symbols[top as usize].flags;
            let k = ex.symbols[i].kind;
            // functions inside a type are methods; inside a `mod`/`namespace` they stay functions
            if pk.is_container() && pk != DefKind::Module {
                if k == DefKind::Function {
                    ex.symbols[i].kind = DefKind::Method;
                } else if k == DefKind::Variable && lang == Lang::Kotlin {
                    ex.symbols[i].kind = DefKind::Field;
                }
            }
            // Rust: everything inside a `#[cfg(test)] mod` is test code
            if lang == Lang::Rust && pk == DefKind::Module && pflags & SYM_TEST != 0 {
                ex.symbols[i].flags |= SYM_TEST;
            }
        }
        let _ = e;
        stack.push(i as u32);
    }
    // noncode: sort, drop nested duplicates, prefer docstring on equal start
    ex.noncode
        .sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let mut out: Vec<Span> = Vec::with_capacity(ex.noncode.len());
    for sp in ex.noncode.drain(..) {
        if let Some(last) = out.last_mut() {
            if sp.start == last.start {
                if sp.kind == SpanKind::Docstring {
                    last.kind = SpanKind::Docstring;
                }
                if sp.end > last.end {
                    last.end = sp.end;
                }
                continue;
            }
            if sp.end <= last.end {
                continue;
            }
        }
        out.push(sp);
    }
    ex.noncode = out;
    // docs: a docstring/comment span ending just before the symbol (whitespace only between), or
    // a Python docstring right after the definition line.
    for s in &mut ex.symbols {
        let before = ex.noncode.partition_point(|n| n.end <= s.start);
        let mut has = false;
        if before > 0 {
            let n = &ex.noncode[before - 1];
            if s.start - n.end < 200
                && (n.kind == SpanKind::Docstring || n.kind == SpanKind::Comment)
                && src[n.end as usize..s.start as usize].iter().all(|b| {
                    b.is_ascii_whitespace()
                        || *b == b'#'
                        || *b == b'@'
                        || *b == b'['
                        || *b == b']'
                        || is_word(*b)
                        || *b == b'('
                        || *b == b')'
                        || *b == b'.'
                        || *b == b'='
                        || *b == b'"'
                        || *b == b','
                })
            {
                has = true;
            }
        }
        if !has && lang == Lang::Python {
            let after = ex.noncode.partition_point(|n| n.start < s.start);
            if let Some(n) = ex.noncode.get(after)
                && n.kind == SpanKind::Docstring
                && n.start < s.end
            {
                has = true;
            }
        }
        if has {
            s.flags |= SYM_HAS_DOC;
        }
        if lang == Lang::Python {
            if python_name_exported(&src[s.name_start as usize..s.name_end as usize]) {
                s.flags |= SYM_EXPORTED;
            } else {
                s.flags &= !SYM_EXPORTED;
            }
        }
    }
    ex.imports.sort_by_key(|i| i.start);
}

/// Regex-based fallback (DESIGN.md §7.5): outline + byte lexer + import lines.
pub fn regex_extract(lang: Lang, src: &[u8]) -> Extract {
    let mut ex = Extract::default();
    if !lang.has_grammar() {
        return ex;
    }
    let lexed = lexer::lex(lang, src);
    let outline = defs::outline(lang, src, &lexed);
    for d in &outline.defs {
        // visibility from the modifiers before the name (Python is fixed up in `finish`)
        let head = crate::trim_start(&src[d.start as usize..d.name_start as usize]);
        let exported = match lang {
            Lang::Rust => head.starts_with(b"pub ") || head.starts_with(b"pub\t"),
            Lang::Kotlin => !head
                .split(|b| b.is_ascii_whitespace())
                .any(|w| w == b"private" || w == b"internal"),
            _ => true,
        };
        ex.symbols.push(Symbol {
            name_start: d.name_start,
            name_end: d.name_end,
            start: d.start,
            end: d.end,
            line: d.line,
            kind: d.kind,
            parent: None,
            flags: if exported { SYM_EXPORTED } else { 0 },
            supers: Vec::new(),
        });
    }
    ex.noncode = lexed.spans;
    let mut pos = 0usize;
    while pos < src.len() {
        let le = memchr::memchr(b'\n', &src[pos..])
            .map(|k| pos + k)
            .unwrap_or(src.len());
        let line = &src[pos..le];
        if crate::is_import_line(lang, line) {
            let t = crate::trim_start(line);
            let off = pos + (line.len() - t.len());
            parse_imports(lang, t, off as u32, le as u32, &mut ex.imports);
        } else if lang == Lang::Kotlin
            && ex.package.is_none()
            && crate::trim_start(line).starts_with(b"package ")
        {
            ex.package = Some(
                String::from_utf8_lossy(crate::trim_start(line)[8..].trim_ascii())
                    .trim_end_matches(';')
                    .to_string(),
            );
        }
        pos = le + 1;
    }
    finish(&mut ex, lang, src);
    ex
}

// ---------------------------------------------------------------- precise kinds

/// Exact hit kinds from the syntax tree (`--precise`, DESIGN.md §3.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Def,
    Import,
    Call,
    Type,
    Member,
    Ident,
    Comment,
    Str,
}

/// A parsed file for repeated `node_kind_at` queries.
pub struct Parsed {
    tree: tree_sitter::Tree,
    lang: Lang,
}

pub fn parse(lang: Lang, tsx: bool, src: &[u8]) -> Option<Parsed> {
    let lq = lang_q(lang, tsx)?;
    let tree = PARSER.with(|p| {
        let mut p = p.borrow_mut();
        p.set_language(&lq.lang).ok()?;
        let t0 = Instant::now();
        let mut cb = |_: &tree_sitter::ParseState| {
            if t0.elapsed() > PARSE_TIMEOUT {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let opts = ParseOptions::new().progress_callback(&mut cb);
        p.parse_with_options(
            &mut |off, _| if off < src.len() { &src[off..] } else { &[] },
            None,
            Some(opts),
        )
    })?;
    Some(Parsed { tree, lang })
}

impl Parsed {
    /// Kind of the identifier covering `[ms, me)`.
    pub fn kind_at(&self, ms: usize, me: usize) -> NodeKind {
        let root = self.tree.root_node();
        let Some(node) = root.descendant_for_byte_range(ms, me.max(ms + 1) - 1) else {
            return NodeKind::Ident;
        };
        let k = node.kind();
        if k.contains("comment") {
            return NodeKind::Comment;
        }
        if k.contains("string")
            || k == "template_string"
            || k == "regex"
            || k == "char_literal"
            || k == "character_literal"
        {
            return NodeKind::Str;
        }
        // walk up through wrappers that carry no meaning (e.g. Kotlin `user_type` inside `delegation_specifier`)
        let Some(parent) = node.parent() else {
            return NodeKind::Ident;
        };
        let pk = parent.kind();
        let is_name_of_parent = parent
            .child_by_field_name("name")
            .map(|n| n.id() == node.id())
            .unwrap_or(false);
        // JS/TS object-literal members implement a typed member more often than
        // they define one (SCIP: a reference to the interface property)
        if matches!(pk, "pair" | "method_definition")
            && parent
                .parent()
                .map(|g| g.kind() == "object")
                .unwrap_or(false)
            && (is_name_of_parent
                || parent
                    .child_by_field_name("key")
                    .map(|n| n.id() == node.id())
                    .unwrap_or(false))
        {
            return NodeKind::Member;
        }
        if is_name_of_parent
            && (pk.ends_with("_declaration")
                || pk.ends_with("_definition")
                || pk.ends_with("_item")
                || pk.ends_with("_signature")
                || pk == "class"
                || pk == "module"
                || pk == "internal_module"
                || pk == "variable_declarator"
                || pk == "pair"
                || pk == "enum_assignment"
                || pk == "field_definition"
                || pk == "public_field_definition"
                || pk == "property_signature")
        {
            return NodeKind::Def;
        }
        if self.lang == Lang::Kotlin
            && matches!(
                pk,
                "class_declaration"
                    | "object_declaration"
                    | "function_declaration"
                    | "variable_declaration"
                    | "type_alias"
                    | "enum_entry"
                    | "class_parameter"
                    | "companion_object"
            )
            && node.kind() != "user_type"
        {
            return NodeKind::Def;
        }
        let gp = parent.parent();
        let gk = gp.map(|g| g.kind()).unwrap_or("");
        let ancestors_contain = |needle: &str| {
            let mut n = Some(parent);
            for _ in 0..4 {
                match n {
                    Some(x) if x.kind().contains(needle) => return true,
                    Some(x) => n = x.parent(),
                    None => break,
                }
            }
            false
        };
        if ancestors_contain("import")
            || ancestors_contain("use_declaration")
            || pk == "extern_crate_declaration"
            || pk == "package_header"
        {
            return NodeKind::Import;
        }
        // calls: callee position of a call node, directly or through a member access
        let is_call_parent = |k: &str| {
            matches!(
                k,
                "call_expression"
                    | "call"
                    | "macro_invocation"
                    | "new_expression"
                    | "constructor_invocation"
                    | "generic_function"
                    | "call_suffix"
            )
        };
        let callee = |p: Node| -> bool {
            p.child_by_field_name("function")
                .map(|f| f.id() == node.id() || f.id() == parent.id())
                .unwrap_or(false)
                || p.child_by_field_name("constructor")
                    .map(|f| f.id() == node.id() || f.id() == parent.id())
                    .unwrap_or(false)
                || p.child_by_field_name("macro")
                    .map(|f| f.id() == node.id())
                    .unwrap_or(false)
        };
        if is_call_parent(pk) && (callee(parent) || self.lang == Lang::Kotlin) {
            return NodeKind::Call;
        }
        if let Some(g) = gp
            && is_call_parent(gk)
            && (callee(g) || (self.lang == Lang::Kotlin && pk == "navigation_suffix"))
        {
            return NodeKind::Call;
        }
        if self.lang == Lang::Kotlin
            && pk == "navigation_suffix"
            && gp
                .and_then(|g| g.parent())
                .map(|c| c.kind() == "call_expression")
                .unwrap_or(false)
        {
            return NodeKind::Call;
        }
        if k == "type_identifier"
            || k == "primitive_type"
            || pk.contains("type") && pk != "type_arguments"
            || gk == "type_annotation"
            || pk == "extends_clause"
            || pk == "implements_clause"
            || pk == "delegation_specifier"
            || pk == "trait_bounds"
            || pk == "superclasses"
            || gk == "generic_type"
            || pk == "type_arguments"
            || pk == "class_heritage"
        {
            return NodeKind::Type;
        }
        if (pk == "member_expression"
            && parent
                .child_by_field_name("property")
                .map(|f| f.id() == node.id())
                .unwrap_or(false))
            || (pk == "field_expression"
                && parent
                    .child_by_field_name("field")
                    .map(|f| f.id() == node.id())
                    .unwrap_or(false))
            || (pk == "attribute"
                && parent
                    .child_by_field_name("attribute")
                    .map(|f| f.id() == node.id())
                    .unwrap_or(false))
            || pk == "navigation_suffix"
            || (pk == "scoped_identifier"
                && parent
                    .child_by_field_name("name")
                    .map(|f| f.id() == node.id())
                    .unwrap_or(false))
            || pk == "field_initializer"
            || pk == "shorthand_field_initializer"
        {
            return NodeKind::Member;
        }
        NodeKind::Ident
    }
}

// ---------------------------------------------------------------- imports

fn ident_at(t: &[u8], mut i: usize) -> (usize, usize) {
    while i < t.len() && t[i].is_ascii_whitespace() {
        i += 1;
    }
    let s = i;
    while i < t.len() && (is_word(t[i]) || t[i] == b'.' || t[i] == b'*') {
        i += 1;
    }
    (s, i)
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Parse one import statement's text into [`Import`]s.
pub fn parse_imports(lang: Lang, t: &[u8], start: u32, end: u32, out: &mut Vec<Import>) {
    let mk = |module: String, names: Vec<String>, wildcard: bool| Import {
        start,
        end,
        module,
        names,
        wildcard,
    };
    match lang {
        Lang::Python => {
            let t = t.trim_ascii();
            if let Some(rest) = t.strip_prefix(b"from ") {
                let (ms, me) = ident_at(rest, 0);
                let module = lossy(&rest[ms..me]);
                let after = &rest[me..];
                let Some(p) = memchr::memmem::find(after, b"import") else {
                    return;
                };
                let list = &after[p + 6..];
                let wildcard = list.contains(&b'*');
                let names = split_names(list);
                out.push(mk(module, names, wildcard));
            } else if let Some(rest) = t.strip_prefix(b"import ") {
                for part in rest.split(|&b| b == b',') {
                    let (s, e) = ident_at(part, 0);
                    if e > s {
                        out.push(mk(lossy(&part[s..e]), vec![], false));
                    }
                }
            }
        }
        Lang::Kotlin => {
            let t = t.trim_ascii();
            let Some(rest) = t.strip_prefix(b"import ") else {
                return;
            };
            let (s, e) = ident_at(rest, 0);
            let path = lossy(&rest[s..e]);
            let wildcard = path.ends_with(".*");
            let module = path.trim_end_matches(".*").to_string();
            let names = if wildcard {
                vec![]
            } else {
                module
                    .rsplit('.')
                    .next()
                    .map(|s| vec![s.to_string()])
                    .unwrap_or_default()
            };
            out.push(mk(module, names, wildcard));
        }
        Lang::Rust => {
            let t = t.trim_ascii();
            if let Some(rest) = t.strip_prefix(b"extern crate ") {
                let (s, e) = ident_at(rest, 0);
                out.push(mk(lossy(&rest[s..e]), vec![], false));
                return;
            }
            if crate::trim_start(t).starts_with(b"mod ")
                || memchr::memmem::find(t, b" mod ").is_some() && !t.contains(&b'{')
            {
                let p = memchr::memmem::find(t, b"mod ").unwrap();
                let (s, e) = ident_at(t, p + 4);
                out.push(mk(format!("self::{}", lossy(&t[s..e])), vec![], false));
                return;
            }
            let Some(p) = memchr::memmem::find(t, b"use ") else {
                return;
            };
            let body = t[p + 4..].trim_ascii().trim_ascii_end();
            let body = body.strip_suffix(b";").unwrap_or(body);
            let mut leaves: Vec<String> = Vec::new();
            expand_use_tree(body, "", &mut leaves);
            for leaf in leaves {
                let wildcard = leaf.ends_with("::*");
                let module = leaf.trim_end_matches("::*").to_string();
                let names = if wildcard {
                    vec![]
                } else {
                    module
                        .rsplit("::")
                        .next()
                        .map(|s| vec![s.to_string()])
                        .unwrap_or_default()
                };
                out.push(mk(module, names, wildcard));
            }
        }
        Lang::JavaScript | Lang::TypeScript => {
            // module = quoted source; names = clause identifiers
            let Some(q) = t.iter().position(|&b| b == b'\'' || b == b'"' || b == b'`') else {
                return;
            };
            let qc = t[q];
            let Some(qe) = t[q + 1..].iter().position(|&b| b == qc) else {
                return;
            };
            let module = lossy(&t[q + 1..q + 1 + qe]);
            let clause = &t[..q];
            let mut names = Vec::new();
            let mut wildcard = false;
            if clause.starts_with(b"import") || clause.starts_with(b"export") {
                let c = &clause[6..];
                let c = memchr::memmem::find(c, b"from")
                    .map(|p| &c[..p])
                    .unwrap_or(c);
                let c = c.strip_prefix(b" type").unwrap_or(c);
                if c.contains(&b'*') {
                    wildcard = true;
                }
                for part in c.split(|&b| b == b',' || b == b'{' || b == b'}') {
                    let part = part.trim_ascii();
                    let part = part.strip_prefix(b"type ").unwrap_or(part);
                    let (s, e) = ident_at(part, 0);
                    let nm = &part[s..e];
                    if !nm.is_empty() && nm != b"*" && nm != b"type" && nm != b"default" {
                        names.push(lossy(nm));
                    }
                }
            }
            out.push(mk(module, names, wildcard));
        }
        _ => {}
    }
}

fn split_names(list: &[u8]) -> Vec<String> {
    let mut v = Vec::new();
    for part in list.split(|&b| b == b',') {
        let part = part.trim_ascii().trim_ascii_start();
        let part: &[u8] = part.strip_prefix(b"(").unwrap_or(part);
        let part = part.strip_suffix(b")").unwrap_or(part);
        let (s, e) = ident_at(part, 0);
        let nm = &part[s..e];
        if !nm.is_empty() && nm != b"*" {
            v.push(lossy(nm));
        }
    }
    v
}

/// `a::{b, c::{d, self}, e as f}` → `a::b`, `a::c::d`, `a::c`, `a::e`.
fn expand_use_tree(t: &[u8], prefix: &str, out: &mut Vec<String>) {
    let t = t.trim_ascii();
    if t.is_empty() {
        return;
    }
    if let Some(brace) = t.iter().position(|&b| b == b'{') {
        let head = lossy(t[..brace].trim_ascii())
            .trim_end_matches("::")
            .to_string();
        let prefix = join_path(prefix, &head);
        let inner = &t[brace + 1..t.iter().rposition(|&b| b == b'}').unwrap_or(t.len())];
        // split at depth-0 commas
        let mut depth = 0;
        let mut s = 0;
        for (i, &b) in inner.iter().enumerate() {
            match b {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                b',' if depth == 0 => {
                    expand_use_tree(&inner[s..i], &prefix, out);
                    s = i + 1;
                }
                _ => {}
            }
        }
        expand_use_tree(&inner[s..], &prefix, out);
        return;
    }
    let s = lossy(t);
    let s = s.split(" as ").next().unwrap_or("").trim();
    let leaf = if s == "self" {
        prefix.to_string()
    } else {
        join_path(prefix, s)
    };
    if !leaf.is_empty() {
        out.push(leaf);
    }
}

fn join_path(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_string()
    } else if b.is_empty() {
        a.to_string()
    } else {
        format!("{a}::{b}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `GREEG_DUMP=path cargo test -p greeg-lang dump_parse_errors -- --ignored --nocapture`:
    /// print every ERROR/MISSING node of a file with its line, for grammar triage.
    /// `GREEG_SEXP='code' GREEG_SEXP_LANG=ts cargo test -p greeg-lang dump_sexp -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dump_sexp() {
        let Ok(code) = std::env::var("GREEG_SEXP") else {
            return;
        };
        let lang = match std::env::var("GREEG_SEXP_LANG").as_deref() {
            Ok("js") => Lang::JavaScript,
            Ok("py") => Lang::Python,
            Ok("rs") => Lang::Rust,
            Ok("kt") => Lang::Kotlin,
            _ => Lang::TypeScript,
        };
        let parsed = parse(lang, false, code.as_bytes()).unwrap();
        println!("{}", parsed.tree.root_node().to_sexp());
    }

    #[test]
    #[ignore]
    fn dump_parse_errors() {
        let Ok(path) = std::env::var("GREEG_DUMP") else {
            return;
        };
        let src = std::fs::read(&path).unwrap();
        let lang = Lang::from_path(std::path::Path::new(&path));
        let parsed = parse(lang, is_tsx(&path), &src).unwrap();
        let mut cur = parsed.tree.walk();
        let mut stack = vec![parsed.tree.root_node()];
        let mut n = 0;
        while let Some(node) = stack.pop() {
            if node.is_error() || node.is_missing() {
                let line = node.start_position().row + 1;
                let s = node.start_byte();
                let e = node.end_byte().min(s + 80);
                println!(
                    "{line}: {} {:?}",
                    node.kind(),
                    String::from_utf8_lossy(&src[s..e])
                );
                n += 1;
                if n > 40 {
                    break;
                }
                continue;
            }
            if node.has_error() {
                for ch in node
                    .children(&mut cur)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                {
                    stack.push(ch);
                }
            }
        }
        println!("{n} error nodes");
    }

    fn names(ex: &Extract, src: &[u8]) -> Vec<(String, &'static str, Option<String>)> {
        ex.symbols
            .iter()
            .map(|s| {
                (
                    ex.name(s, src).to_string(),
                    s.kind.name(),
                    s.parent
                        .map(|p| ex.name(&ex.symbols[p as usize], src).to_string()),
                )
            })
            .collect()
    }

    #[test]
    fn queries_compile() {
        warm();
    }

    #[test]
    fn python() {
        let src = b"\"\"\"module doc\"\"\"\nimport os, sys\nfrom a.b import (c, d as e)\nfrom . import f\nMAX = 3\n\nclass A(Base, metaclass=M):\n    \"\"\"doc\"\"\"\n    x = 1\n    def m(self):\n        y = 2\n        return y\n\ndef free():\n    pass\n";
        let ex = extract(Lang::Python, false, src);
        assert!(ex.tree_sitter);
        let n = names(&ex, src);
        assert_eq!(
            n,
            vec![
                ("MAX".into(), "const", None),
                ("A".into(), "class", None),
                ("x".into(), "field", Some("A".into())),
                ("m".into(), "method", Some("A".into())),
                ("free".into(), "fn", None),
            ]
        );
        let a = &ex.symbols[1];
        assert_eq!(
            a.supers
                .iter()
                .map(|&(s, e)| std::str::from_utf8(&src[s as usize..e as usize]).unwrap())
                .collect::<Vec<_>>(),
            vec!["Base"]
        );
        assert!(a.flags & SYM_HAS_DOC != 0);
        assert_eq!(ex.imports.len(), 4);
        assert_eq!(ex.imports[0].module, "os");
        assert_eq!(
            ex.imports[2],
            Import {
                start: ex.imports[2].start,
                end: ex.imports[2].end,
                module: "a.b".into(),
                names: vec!["c".into(), "d".into()],
                wildcard: false
            }
        );
        assert_eq!(ex.imports[3].module, ".");
        assert_eq!(
            ex.noncode
                .iter()
                .filter(|s| s.kind == SpanKind::Docstring)
                .count(),
            2
        );
    }

    #[test]
    fn rust() {
        let src = b"//! crate doc\nuse std::{io, fmt::{self, Debug}};\nmod sub;\n/// S doc\npub struct S { a: u8 }\nimpl Debug for S {\n    fn fmt(&self) {}\n}\nimpl S {\n    pub fn new() -> S { S { a: 0 } }\n}\npub trait T: Send + Sync {\n    type Out;\n    fn req(&self);\n}\nenum E { A, B(u8) }\nconst C: u8 = 1;\nmacro_rules! m { () => {} }\nfn free() { let s = \"x\"; }\n";
        let ex = extract(Lang::Rust, false, src);
        assert!(ex.tree_sitter, "{ex:?}");
        let n = names(&ex, src);
        let want: Vec<(&str, &str, Option<&str>)> = vec![
            ("sub", "mod", None),
            ("S", "struct", None),
            ("a", "field", Some("S")),
            ("S", "impl", None),
            ("fmt", "method", Some("S")),
            ("S", "impl", None),
            ("new", "method", Some("S")),
            ("T", "trait", None),
            ("Out", "type", Some("T")),
            ("req", "method", Some("T")),
            ("E", "enum", None),
            ("A", "variant", Some("E")),
            ("B", "variant", Some("E")),
            ("C", "const", None),
            ("m", "macro", None),
            ("free", "fn", None),
        ];
        assert_eq!(
            n.iter()
                .map(|(a, b, c)| (a.as_str(), *b, c.as_deref()))
                .collect::<Vec<_>>(),
            want
        );
        let sup = |i: usize| {
            ex.symbols[i]
                .supers
                .iter()
                .map(|&(s, e)| std::str::from_utf8(&src[s as usize..e as usize]).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(sup(3), vec!["Debug"]);
        assert_eq!(sup(7), vec!["Send", "Sync"]);
        assert!(ex.symbols[1].flags & SYM_EXPORTED != 0 && ex.symbols[1].flags & SYM_HAS_DOC != 0);
        assert!(ex.symbols[2].flags & SYM_EXPORTED == 0);
        let mods: Vec<&str> = ex.imports.iter().map(|i| i.module.as_str()).collect();
        assert_eq!(
            mods,
            vec!["std::io", "std::fmt", "std::fmt::Debug", "self::sub"]
        );
    }

    #[test]
    fn typescript() {
        let src = b"import x, { a, b as c } from './m';\nimport type { T } from \"./t\";\nexport * from './re';\nconst r = require('./cjs');\n/** doc */\nexport class K<T> extends Base<T> implements I, J {\n  private p = 1;\n  static s(): void {}\n  m() { return `t`; }\n}\nexport interface I extends Q { f: number; g(): void }\ntype Al = string;\nenum En { A, B = 2 }\nexport const arrow = async (x: number) => x;\nconst v = 3;\nfunction f() { const inner = () => 1; }\nnamespace NS { export function g() {} }\n";
        let ex = extract(Lang::TypeScript, false, src);
        assert!(ex.tree_sitter);
        let n = names(&ex, src);
        let want: Vec<(&str, &str, Option<&str>)> = vec![
            ("r", "var", None),
            ("K", "class", None),
            ("p", "field", Some("K")),
            ("s", "method", Some("K")),
            ("m", "method", Some("K")),
            ("I", "interface", None),
            ("f", "field", Some("I")),
            ("g", "method", Some("I")),
            ("Al", "type", None),
            ("En", "enum", None),
            ("A", "variant", Some("En")),
            ("B", "variant", Some("En")),
            ("arrow", "fn", None),
            ("v", "var", None),
            ("f", "fn", None),
            ("NS", "mod", None),
            ("g", "fn", Some("NS")),
        ];
        assert_eq!(
            n.iter()
                .map(|(a, b, c)| (a.as_str(), *b, c.as_deref()))
                .collect::<Vec<_>>(),
            want
        );
        let sup = |i: usize| {
            ex.symbols[i]
                .supers
                .iter()
                .map(|&(s, e)| std::str::from_utf8(&src[s as usize..e as usize]).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(sup(1), vec!["Base", "I", "J"]);
        assert_eq!(sup(5), vec!["Q"]);
        assert!(ex.symbols[1].flags & SYM_EXPORTED != 0 && ex.symbols[1].flags & SYM_HAS_DOC != 0);
        assert!(ex.symbols[2].flags & SYM_EXPORTED != 0); // members count as exported
        assert!(ex.symbols[13].flags & SYM_EXPORTED == 0);
        let imps: Vec<(&str, Vec<&str>)> = ex
            .imports
            .iter()
            .map(|i| {
                (
                    i.module.as_str(),
                    i.names.iter().map(|s| s.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            imps,
            vec![
                ("./m", vec!["x", "a", "b"]),
                ("./t", vec!["T"]),
                ("./re", vec![]),
                ("./cjs", vec![])
            ]
        );
        assert!(ex.imports[2].wildcard);
    }

    #[test]
    fn tsx_and_js() {
        let src = b"export default function App() { return <div className=\"x\">{1}</div>; }\nconst C = () => <App/>;\n";
        let ex = extract(Lang::TypeScript, true, src);
        assert!(ex.tree_sitter && !ex.parse_errors);
        assert_eq!(
            names(&ex, src)
                .iter()
                .map(|x| x.0.as_str())
                .collect::<Vec<_>>(),
            vec!["App", "C"]
        );
        let src = b"const fs = require('fs');\nmodule.exports.run = function () {};\nclass Q extends P { #priv() {} go() {} }\nfunction* gen() {}\nvar old = function () {};\n";
        let ex = extract(Lang::JavaScript, false, src);
        assert!(ex.tree_sitter, "{ex:?}");
        let n = names(&ex, src);
        assert_eq!(
            n.iter()
                .map(|(a, b, _)| (a.as_str(), *b))
                .collect::<Vec<_>>(),
            vec![
                ("fs", "var"),
                ("run", "fn"),
                ("Q", "class"),
                ("priv", "method"),
                ("go", "method"),
                ("gen", "fn"),
                ("old", "fn")
            ]
        );
        assert_eq!(ex.imports[0].module, "fs");
    }

    #[test]
    fn kotlin() {
        let src = b"package a.b\n\nimport c.d.E\nimport c.d.*\n\n/** doc */\n@Serializable\ndata class P(val x: Int, y: Int) : Base(), I {\n    val z = 1\n    fun m() = z\n    companion object { const val K = 2 }\n    constructor(s: String) : this(1, 2)\n}\nobject O : I\nenum class En { A, B }\ntypealias Al = P\nfun top(): Int = \"s\".length\ninterface I {\n    fun req()\n}\n";
        let ex = extract(Lang::Kotlin, false, src);
        assert!(ex.tree_sitter, "{ex:?}");
        let n = names(&ex, src);
        let want: Vec<(&str, &str, Option<&str>)> = vec![
            ("P", "class", None),
            ("x", "field", Some("P")),
            ("z", "field", Some("P")),
            ("m", "method", Some("P")),
            ("companion", "object", Some("P")),
            ("K", "field", Some("companion")),
            ("constructor", "method", Some("P")),
            ("O", "object", None),
            ("En", "enum", None),
            ("A", "variant", Some("En")),
            ("B", "variant", Some("En")),
            ("Al", "type", None),
            ("top", "fn", None),
            ("I", "interface", None),
            ("req", "method", Some("I")),
        ];
        assert_eq!(
            n.iter()
                .map(|(a, b, c)| (a.as_str(), *b, c.as_deref()))
                .collect::<Vec<_>>(),
            want
        );
        let sup = |i: usize| {
            ex.symbols[i]
                .supers
                .iter()
                .map(|&(s, e)| std::str::from_utf8(&src[s as usize..e as usize]).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(sup(0), vec!["Base", "I"]);
        assert_eq!(ex.package.as_deref(), Some("a.b"));
        assert_eq!(
            ex.symbols[0].line, 8,
            "line is the name's line, not the annotation's"
        );
        assert_eq!(
            ex.imports
                .iter()
                .map(|i| (i.module.as_str(), i.wildcard))
                .collect::<Vec<_>>(),
            vec![("c.d.E", false), ("c.d", true)]
        );
        assert!(ex.symbols[0].flags & SYM_HAS_DOC != 0);
    }

    #[test]
    fn rust_macro_bodies_and_tuple_impls() {
        let src = b"cfg_rt! {\n    /// Doc\n    pub struct J<T> { raw: T }\n    impl<T> J<T> {\n        pub fn new() {}\n    }\n}\nimpl Tr for (A, u16) {\n    fn m(&self) {}\n}\nimpl Tr for [u8; 4] {}\nprintln!(\"fn not an item\");\n";
        let ex = extract(Lang::Rust, false, src);
        let n = names(&ex, src);
        let want: Vec<(&str, &str, Option<&str>)> = vec![
            ("J", "struct", None),
            ("raw", "field", Some("J")),
            ("J", "impl", None),
            ("new", "method", Some("J")),
            ("Tr", "impl", None),
            ("m", "method", Some("Tr")),
            ("Tr", "impl", None),
        ];
        assert_eq!(
            n.iter()
                .map(|(a, b, c)| (a.as_str(), *b, c.as_deref()))
                .collect::<Vec<_>>(),
            want
        );
        assert_eq!(ex.symbols[0].line, 3);
        assert!(ex.symbols[0].flags & SYM_HAS_DOC != 0);
        assert!(ex.symbols[0].flags & SYM_EXPORTED != 0);
    }

    fn flags_of<'a>(ex: &Extract, src: &'a [u8]) -> Vec<(&'a str, u8)> {
        ex.symbols
            .iter()
            .map(|s| {
                (
                    ex.name(s, src),
                    s.flags & (SYM_EXPORTED | SYM_TEST | SYM_OBJ_MEMBER),
                )
            })
            .collect()
    }

    #[test]
    fn rust_visibility_is_pub_only() {
        let src = b"pub struct A;\npub(crate) struct B;\npub(super) struct C;\npub(in crate::x) struct D;\nstruct E;\npub fn f() {}\npub(crate) fn g() {}\n";
        let ex = extract(Lang::Rust, false, src);
        assert!(ex.tree_sitter);
        let v: Vec<(&str, bool)> = flags_of(&ex, src)
            .iter()
            .map(|(n, f)| (*n, f & SYM_EXPORTED != 0))
            .collect();
        assert_eq!(
            v,
            vec![
                ("A", true),
                ("B", false),
                ("C", false),
                ("D", false),
                ("E", false),
                ("f", true),
                ("g", false)
            ]
        );
        // regex fallback agrees
        let ex = regex_extract(Lang::Rust, src);
        let v: Vec<(&str, bool)> = flags_of(&ex, src)
            .iter()
            .map(|(n, f)| (*n, f & SYM_EXPORTED != 0))
            .collect();
        assert_eq!(
            v,
            vec![
                ("A", true),
                ("B", false),
                ("C", false),
                ("D", false),
                ("E", false),
                ("f", true),
                ("g", false)
            ]
        );
    }

    #[test]
    fn rust_test_attributes() {
        let src = b"#[cfg(not(test))]\nfn a() {}\n#[cfg_attr(test, derive(Debug))]\nstruct B;\n#[test]\nfn c() {}\n#[tokio::test(flavor = \"multi_thread\")]\nasync fn d() {}\n#[rstest]\n#[case(1)]\nfn e() {}\n#[wasm_bindgen_test]\nfn w() {}\n#[async_std::test]\nfn s() {}\n#[cfg(test)]\nmod tests {\n    fn helper() {}\n    struct Fx;\n}\n#[cfg(test)]\nfn not_a_mod() {}\nmod plain {\n    fn p() {}\n}\n";
        let ex = extract(Lang::Rust, false, src);
        assert!(ex.tree_sitter, "{ex:?}");
        let v: Vec<(&str, bool)> = flags_of(&ex, src)
            .iter()
            .map(|(n, f)| (*n, f & SYM_TEST != 0))
            .collect();
        assert_eq!(
            v,
            vec![
                ("a", false),
                ("B", false),
                ("c", true),
                ("d", true),
                ("e", true),
                ("w", true),
                ("s", true),
                ("tests", true),
                ("helper", true),
                ("Fx", true),
                ("not_a_mod", false),
                ("plain", false),
                ("p", false)
            ]
        );
    }

    #[test]
    fn mod_functions_stay_functions() {
        let src = b"mod x {\n    fn f() {}\n    struct S;\n    impl S {\n        fn m(&self) {}\n    }\n}\n";
        let ex = extract(Lang::Rust, false, src);
        let n = names(&ex, src);
        assert_eq!(
            n.iter()
                .map(|(a, b, c)| (a.as_str(), *b, c.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("x", "mod", None),
                ("f", "fn", Some("x")),
                ("S", "struct", Some("x")),
                ("S", "impl", Some("x")),
                ("m", "method", Some("S"))
            ]
        );
        let src = b"namespace N {\n  function f() {}\n  export class C {\n    m() {}\n  }\n}\n";
        let ex = extract(Lang::TypeScript, false, src);
        let n = names(&ex, src);
        assert_eq!(
            n.iter()
                .map(|(a, b, c)| (a.as_str(), *b, c.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("N", "mod", None),
                ("f", "fn", Some("N")),
                ("C", "class", Some("N")),
                ("m", "method", Some("C"))
            ]
        );
    }

    #[test]
    fn python_visibility_and_constants() {
        let src = b"MAX = 3\n_CACHE = {}\nlower = 1\n__all__ = []\ndef _private(): pass\ndef __init__(self): pass\ndef __mangled(): pass\nclass _Hidden: pass\n";
        let ex = extract(Lang::Python, false, src);
        assert!(ex.tree_sitter);
        let n = names(&ex, src);
        assert_eq!(
            n.iter()
                .map(|(a, b, _)| (a.as_str(), *b))
                .collect::<Vec<_>>(),
            vec![
                ("MAX", "const"),
                ("_CACHE", "const"),
                ("lower", "var"),
                ("__all__", "var"),
                ("_private", "fn"),
                ("__init__", "fn"),
                ("__mangled", "fn"),
                ("_Hidden", "class")
            ]
        );
        let v: Vec<(&str, bool)> = flags_of(&ex, src)
            .iter()
            .map(|(n, f)| (*n, f & SYM_EXPORTED != 0))
            .collect();
        assert_eq!(
            v,
            vec![
                ("MAX", true),
                ("_CACHE", false),
                ("lower", true),
                ("__all__", true),
                ("_private", false),
                ("__init__", true),
                ("__mangled", false),
                ("_Hidden", false)
            ]
        );
        // regex path: same kind for MAX and same visibility
        let ex = regex_extract(Lang::Python, src);
        let n = names(&ex, src);
        assert_eq!(n[0], ("MAX".to_string(), "const", None));
        assert!(
            ex.symbols
                .iter()
                .zip(flags_of(&ex, src))
                .all(
                    |(s, (nm, f))| (f & SYM_EXPORTED != 0) == python_name_exported(nm.as_bytes())
                        || s.kind == DefKind::Constant
                )
        );
    }

    #[test]
    fn kotlin_visibility_kinds_and_locals() {
        let src = b"private class P\ninternal fun h() {}\nprotected class Q\nclass R {\n    private val secret = 1\n    val open = 2\n    fun m() {\n        val local = 3\n        listOf(1).map { val inLambda = it }\n    }\n    init {\n        val inInit = 4\n    }\n    companion object Named {}\n}\nfun interface Fi {\n    fun run()\n}\nsealed interface SI\n@Serializable\nenum class E { A }\n@Suppress(\"interface\")\nclass NotAnInterface\nannotation class An\n";
        let ex = extract(Lang::Kotlin, false, src);
        assert!(ex.tree_sitter, "{ex:?}");
        let n = names(&ex, src);
        let got: Vec<(&str, &str)> = n.iter().map(|(a, b, _)| (a.as_str(), *b)).collect();
        assert_eq!(
            got,
            vec![
                ("P", "class"),
                ("h", "fn"),
                ("Q", "class"),
                ("R", "class"),
                ("secret", "field"),
                ("open", "field"),
                ("m", "method"),
                ("Named", "object"),
                ("Fi", "interface"),
                ("run", "method"),
                ("SI", "interface"),
                ("E", "enum"),
                ("A", "variant"),
                ("NotAnInterface", "class"),
                ("An", "class")
            ]
        );
        let v: Vec<(&str, bool)> = flags_of(&ex, src)
            .iter()
            .take(6)
            .map(|(n, f)| (*n, f & SYM_EXPORTED != 0))
            .collect();
        assert_eq!(
            v,
            vec![
                ("P", false),
                ("h", false),
                ("Q", true),
                ("R", true),
                ("secret", false),
                ("open", true)
            ]
        );
        let ex = regex_extract(Lang::Kotlin, src);
        let v: Vec<(&str, bool)> = flags_of(&ex, src)
            .iter()
            .take(3)
            .map(|(n, f)| (*n, f & SYM_EXPORTED != 0))
            .collect();
        assert_eq!(v, vec![("P", false), ("h", false), ("Q", true)]);
    }

    #[test]
    fn ts_ambient_declarations() {
        let src = b"declare const X: number;\ndeclare var Y: string;\ndeclare function df(): void;\ndeclare class DC {}\ndeclare module 'my-lib' {\n  export function g(): void;\n}\nexport declare const EX: string;\nexport declare function ef(): void;\n";
        let ex = extract(Lang::TypeScript, false, src);
        assert!(ex.tree_sitter, "{ex:?}");
        let n = names(&ex, src);
        assert_eq!(
            n.iter()
                .map(|(a, b, c)| (a.as_str(), *b, c.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("X", "var", None),
                ("Y", "var", None),
                ("df", "fn", None),
                ("DC", "class", None),
                ("my-lib", "mod", None),
                ("g", "fn", Some("my-lib")),
                ("EX", "var", None),
                ("ef", "fn", None)
            ]
        );
        let v: Vec<(&str, bool)> = flags_of(&ex, src)
            .iter()
            .map(|(n, f)| (*n, f & SYM_EXPORTED != 0))
            .collect();
        assert_eq!(v[0], ("X", false));
        assert_eq!(v[6], ("EX", true));
        assert_eq!(v[7], ("ef", true));
    }

    /// `exported` never leaks through a function or class body: a helper
    /// declared inside `export function f` is a closure, not an export.
    #[test]
    fn ts_exported_stops_at_function_body() {
        let src = b"export function outer() {\n  function inner() {}\n  const arrow = () => 1;\n  return { inner, arrow };\n}\nexport const top = () => 2;\nexport class C {\n  m() { function deep() {} return deep; }\n}\n";
        let ex = extract(Lang::TypeScript, false, src);
        assert!(ex.tree_sitter, "{ex:?}");
        let v: Vec<(&str, bool)> = flags_of(&ex, src)
            .iter()
            .map(|(n, f)| (*n, f & SYM_EXPORTED != 0))
            .collect();
        assert!(v.contains(&("outer", true)), "{v:?}");
        assert!(v.contains(&("inner", false)), "{v:?}");
        assert!(v.contains(&("top", true)), "{v:?}");
        assert!(v.contains(&("C", true)), "{v:?}");
        assert!(v.contains(&("m", true)), "{v:?}");
        assert!(v.contains(&("deep", false)), "{v:?}");
    }

    #[test]
    fn js_import_shapes() {
        let src = b"import type { A } from './a';\nimport B, { c as d } from \"./b\";\nexport * from './e';\nexport { f } from './f';\nconst g = require('./g');\nconst Lazy = React.lazy(() => import('./lazy'));\nasync function load() { const { h } = await import(\"@/h\"); return h; }\n";
        let ex = extract(Lang::TypeScript, false, src);
        assert!(ex.tree_sitter && !ex.parse_errors, "{ex:?}");
        let mods: Vec<(&str, bool)> = ex
            .imports
            .iter()
            .map(|i| (i.module.as_str(), i.wildcard))
            .collect();
        assert_eq!(
            mods,
            vec![
                ("./a", false),
                ("./b", false),
                ("./e", true),
                ("./f", false),
                ("./g", false),
                ("./lazy", false),
                ("@/h", false)
            ]
        );
        assert_eq!(ex.imports[0].names, vec!["A"]);
        assert_eq!(ex.imports[1].names, vec!["B", "c"]);
        assert!(ex.imports[5].names.is_empty());
    }

    #[test]
    fn ts_namespace_level_declarations() {
        let src = b"module M {\n  var a = 1;\n  export var b = 2;\n  const c = () => 1;\n  function f() { var local = 3; }\n}\nnamespace N { let d: string; }\n";
        let ex = extract(Lang::TypeScript, false, src);
        assert!(ex.tree_sitter && !ex.parse_errors, "{ex:?}");
        let n = names(&ex, src);
        assert_eq!(
            n.iter()
                .map(|(a, b, c)| (a.as_str(), *b, c.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("M", "mod", None),
                ("a", "var", Some("M")),
                ("b", "var", Some("M")),
                ("c", "fn", Some("M")),
                ("f", "fn", Some("M")),
                ("N", "mod", None),
                ("d", "var", Some("N")),
            ]
        );
    }

    #[test]
    fn ts_object_literal_members_are_flagged() {
        let src = b"export const host = {\n  watchFile: (f: string) => 1,\n  getEnv(name: string) { return name; },\n  plain: 3,\n};\nclass K { m() {} }\nfunction top() {}\nmodule.exports.run = function () {};\n";
        let ex = extract(Lang::TypeScript, false, src);
        assert!(ex.tree_sitter && !ex.parse_errors, "{ex:?}");
        let v: Vec<(&str, bool)> = flags_of(&ex, src)
            .iter()
            .map(|(n, f)| (*n, f & SYM_OBJ_MEMBER != 0))
            .collect();
        assert!(v.contains(&("host", false)), "{v:?}");
        assert!(v.contains(&("watchFile", true)), "{v:?}");
        assert!(v.contains(&("getEnv", true)), "{v:?}");
        assert!(v.contains(&("m", false)), "{v:?}");
        assert!(v.contains(&("top", false)), "{v:?}");
        assert!(v.contains(&("run", false)), "{v:?}");
        let p = parse(Lang::TypeScript, false, src).unwrap();
        let at = |needle: &str, len: usize| {
            let i = src
                .windows(needle.len())
                .position(|w| w == needle.as_bytes())
                .unwrap();
            p.kind_at(i, i + len)
        };
        assert_eq!(at("watchFile", 9), NodeKind::Member);
        assert_eq!(at("getEnv", 6), NodeKind::Member);
        assert_eq!(at("top", 3), NodeKind::Def);
        assert_eq!(at("m()", 1), NodeKind::Def);
    }

    #[test]
    fn fallback_on_garbage() {
        let src = b"fn ok() {}\n)))))) {{{{ ((((( fn broken( {{{{{{{{{{{{{{{{{{{{{{{{{{ )))))))))))))))))))) ]]]]]]]]]]]]]]]]\n";
        let ex = extract(Lang::Rust, false, src);
        assert!(ex.parse_errors);
        assert!(names(&ex, src).iter().any(|x| x.0 == "ok"));
    }

    #[test]
    fn use_tree() {
        let mut v = Vec::new();
        expand_use_tree(b"a::{b, c::{d, self}, e as f}", "", &mut v);
        assert_eq!(v, vec!["a::b", "a::c::d", "a::c", "a::e"]);
        let mut v = Vec::new();
        expand_use_tree(b"crate::x::*", "", &mut v);
        assert_eq!(v, vec!["crate::x::*"]);
    }
}

#[cfg(test)]
mod dump {
    /// `GREEG_DUMP=path cargo test -p greeg-lang dump -- --ignored --nocapture` prints the tree.
    #[test]
    #[ignore]
    fn tree() {
        let path = std::env::var("GREEG_DUMP").unwrap();
        let src = std::fs::read(&path).unwrap();
        let lang = crate::Lang::from_path(std::path::Path::new(&path));
        let lq = super::lang_q(lang, super::is_tsx(&path)).expect("grammar");
        let mut p = tree_sitter::Parser::new();
        p.set_language(&lq.lang).unwrap();
        let t = p.parse(&src, None).unwrap();
        fn walk(n: tree_sitter::Node, depth: usize, out: &mut String) {
            let field = n.parent().and_then(|p| {
                let mut c = p.walk();
                c.goto_first_child();
                loop {
                    if c.node().id() == n.id() {
                        break c.field_name();
                    }
                    if !c.goto_next_sibling() {
                        break None;
                    }
                }
            });
            out.push_str(&format!(
                "{}{}{}{}\n",
                "  ".repeat(depth),
                field.map(|f| format!("{f}: ")).unwrap_or_default(),
                if n.is_named() {
                    n.kind().to_string()
                } else {
                    format!("{:?}", n.kind())
                },
                if n.child_count() == 0 {
                    format!(" @{}", n.start_position().row + 1)
                } else {
                    String::new()
                }
            ));
            let mut c = n.walk();
            for ch in n.children(&mut c) {
                walk(ch, depth + 1, out);
            }
        }
        let mut out = String::new();
        walk(t.root_node(), 0, &mut out);
        println!("{out}");
    }
}
