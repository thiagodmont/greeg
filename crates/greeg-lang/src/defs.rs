//! Regex-based definition extraction (DESIGN.md §7.5) and enclosing-symbol
//! ranges. Used in scan mode and as the fallback when a grammar fails.

use crate::lexer::{Lexed, SpanKind};
use crate::{DefKind, Lang};
use regex::bytes::{Regex, RegexBuilder};
use std::sync::LazyLock;

#[derive(Clone, Debug)]
pub struct Def {
    pub name_start: u32,
    pub name_end: u32,
    /// Start of the definition line (byte offset).
    pub start: u32,
    /// End of the body (exclusive byte offset).
    pub end: u32,
    pub line: u32,
    pub indent: u16,
    pub kind: DefKind,
    /// Index into `Outline::defs` of the enclosing definition.
    pub parent: Option<u32>,
}

#[derive(Debug, Default)]
pub struct Outline {
    pub defs: Vec<Def>,
}

impl Outline {
    /// Innermost definition whose body contains `off`.
    pub fn enclosing(&self, off: u32) -> Option<u32> {
        // defs sorted by start; walk candidates whose start <= off, pick the
        // innermost (deepest) containing one. Bounded by nesting depth in practice.
        // defs are sorted by start; the innermost containing def is the last
        // one starting at or before `off` whose body still covers it.
        let upto = self.defs.partition_point(|d| d.start <= off);
        (0..upto).rev().find(|&i| self.defs[i].end > off).map(|i| i as u32)
    }
    /// Definition whose *name* overlaps the byte range `[ms, me)`.
    pub fn def_named_in(&self, ms: u32, me: u32) -> Option<u32> {
        let upto = self.defs.partition_point(|d| d.start < me);
        for i in (0..upto).rev() {
            let d = &self.defs[i];
            if d.name_start < me && ms < d.name_end {
                return Some(i as u32);
            }
            if d.start + 512 < ms {
                break;
            }
        }
        None
    }
    /// "Outer › Inner" chain for display.
    pub fn chain(&self, idx: u32, src: &[u8]) -> Vec<(DefKind, String)> {
        let mut v = Vec::new();
        let mut cur = Some(idx);
        let mut guard = 0;
        while let Some(i) = cur {
            let d = &self.defs[i as usize];
            v.push((d.kind, String::from_utf8_lossy(&src[d.name_start as usize..d.name_end as usize]).into_owned()));
            cur = d.parent;
            guard += 1;
            if guard > 16 {
                break;
            }
        }
        v.reverse();
        v
    }
}

struct LangRes {
    re: Regex,
    kinds: Vec<(&'static str, DefKind)>,
}

/// ASCII-only `\w`/`\s`: identifiers in the five languages are ASCII in
/// practice, and Unicode classes make the lazy DFA an order of magnitude
/// more expensive on lines it has not seen before (measured 14 µs vs 1 µs).
fn build(pat: &str) -> Regex {
    RegexBuilder::new(pat).unicode(false).build().unwrap()
}

static PY: LazyLock<LangRes> = LazyLock::new(|| LangRes {
    re: build(r"(?m)^(?P<indent>[ \t]*)(?:(?:async\s+)?(?P<def>def)\s+(?P<name>[A-Za-z_]\w*)|(?P<class>class)\s+(?P<name2>[A-Za-z_]\w*)|(?P<const>[A-Z_][A-Z0-9_]{2,})\s*(?::[^=\n]+)?=[^=])"),
    kinds: vec![("def", DefKind::Function), ("class", DefKind::Class), ("const", DefKind::Constant)],
});
static RS: LazyLock<LangRes> = LazyLock::new(|| LangRes {
    re: build(r#"(?m)^(?P<indent>[ \t]*)(?:pub(?:\([^)]*\))?\s+)?(?:(?:async|const|unsafe|default|extern\s+"[^"]*"|extern)\s+)*(?:(?P<fn>fn)\s+(?P<name>[A-Za-z_]\w*)|(?P<struct>struct)\s+(?P<name2>[A-Za-z_]\w*)|(?P<enum>enum)\s+(?P<name3>[A-Za-z_]\w*)|(?P<trait>trait)\s+(?P<name4>[A-Za-z_]\w*)|(?P<type>type)\s+(?P<name5>[A-Za-z_]\w*)|(?P<mod>mod)\s+(?P<name6>[A-Za-z_]\w*)|(?P<const>const|static)\s+(?:mut\s+)?(?P<name7>[A-Za-z_]\w*)|(?P<macro>macro_rules!)\s+(?P<name8>[A-Za-z_]\w*)|(?P<union>union)\s+(?P<name9>[A-Za-z_]\w*)|(?P<impl>impl)(?:\s*<[^>]*>)?\s+(?:(?P<for_trait>[\w:]+(?:<[^>]*>)?)\s+for\s+)?(?P<name10>[A-Za-z_][\w:]*))"#),
    kinds: vec![
        ("fn", DefKind::Function), ("struct", DefKind::Struct), ("enum", DefKind::Enum), ("trait", DefKind::Trait), ("type", DefKind::TypeAlias),
        ("mod", DefKind::Module), ("const", DefKind::Constant), ("macro", DefKind::Macro), ("union", DefKind::Struct), ("impl", DefKind::Impl),
    ],
});
static JS: LazyLock<LangRes> = LazyLock::new(|| LangRes {
    re: build(r"(?m)^(?P<indent>[ \t]*)(?:export\s+)?(?:default\s+)?(?:declare\s+)?(?:abstract\s+)?(?:(?:async\s+)?(?P<fn>function\*?)\s+(?P<name>[A-Za-z_$][\w$]*)|(?P<class>class)\s+(?P<name2>[A-Za-z_$][\w$]*)|(?P<iface>interface)\s+(?P<name3>[A-Za-z_$][\w$]*)|(?P<type>type)\s+(?P<name4>[A-Za-z_$][\w$]*)\s*(?:<[^=]*>)?\s*=|(?P<enum>(?:const\s+)?enum)\s+(?P<name5>[A-Za-z_$][\w$]*)|(?P<ns>namespace|module)\s+(?P<name6>[A-Za-z_$][\w$.]*)|(?P<arrow>const|let|var)\s+(?P<name7>[A-Za-z_$][\w$]*)\s*(?::[^=]*)?=\s*(?:async\s+)?(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*(?::[^=]*)?=>|(?P<var>const|let|var)\s+(?P<name8>[A-Za-z_$][\w$]*)|(?:(?:public|private|protected|static|readonly|async|override|get|set|abstract|declare)\s+)*(?P<method>[A-Za-z_$][\w$]*)\s*(?:<[^>]*>)?\s*\([^)]*\)\s*(?::\s*[^{;=]+)?\s*\{)"),
    kinds: vec![
        ("fn", DefKind::Function), ("class", DefKind::Class), ("iface", DefKind::Interface), ("type", DefKind::TypeAlias), ("enum", DefKind::Enum),
        ("ns", DefKind::Module), ("arrow", DefKind::Function), ("var", DefKind::Variable), ("method", DefKind::Method),
    ],
});
static KT: LazyLock<LangRes> = LazyLock::new(|| LangRes {
    re: build(r"(?m)^(?P<indent>[ \t]*)(?:(?:public|private|protected|internal|open|abstract|final|sealed|data|enum|annotation|inner|inline|value|suspend|operator|infix|override|external|tailrec|actual|expect|lateinit|const|vararg|crossinline|noinline|context\([^)]*\))\s+)*(?:(?P<fun>fun)(?:\s*<[^>]*>)?\s+(?:[\w.<>?*, ]+?\.)??(?P<name>[A-Za-z_]\w*|`[^`]+`)\s*(?:<|\()|(?P<class>class|interface)\s+(?P<name2>[A-Za-z_]\w*)|(?P<object>object)\s+(?P<name3>[A-Za-z_]\w*)|(?P<companion>companion\s+object)\b\s*(?P<name4>[A-Za-z_]\w*)?|(?P<val>val|var)\s+(?:<[^>]*>\s*)?(?:[\w.<>?]+\.)?(?P<name5>[A-Za-z_]\w*|`[^`]+`)|(?P<typealias>typealias)\s+(?P<name6>[A-Za-z_]\w*))"),
    kinds: vec![
        ("fun", DefKind::Function), ("class", DefKind::Class), ("object", DefKind::Object), ("companion", DefKind::Object), ("val", DefKind::Variable), ("typealias", DefKind::TypeAlias),
    ],
});

const JS_KEYWORDS: &[&str] = &["if", "for", "while", "switch", "catch", "function", "return", "else", "do", "try", "with", "new", "typeof", "await", "yield", "constructor_"];

fn res_for(lang: Lang) -> Option<&'static LangRes> {
    Some(match lang {
        Lang::Python => &PY,
        Lang::Rust => &RS,
        Lang::JavaScript | Lang::TypeScript => &JS,
        Lang::Kotlin => &KT,
        _ => return None,
    })
}

/// Compile the definition regexes now (a few ms); call on a helper thread so
/// the first classification does not block on it.
pub fn warm() {
    let _ = (&*PY, &*RS, &*JS, &*KT);
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Definition keywords and the modifiers that may precede them, per language.
fn is_def_word(lang: Lang, w: &[u8]) -> bool {
    match lang {
        Lang::Python => matches!(w, b"def" | b"class" | b"async"),
        Lang::Rust => matches!(w, b"fn" | b"pub" | b"struct" | b"enum" | b"trait" | b"type" | b"mod" | b"const" | b"static" | b"macro_rules!" | b"union" | b"impl" | b"async" | b"unsafe" | b"extern" | b"default"),
        Lang::JavaScript | Lang::TypeScript => matches!(
            w,
            b"function" | b"class" | b"interface" | b"type" | b"enum" | b"namespace" | b"module" | b"const" | b"let" | b"var" | b"export" | b"default" | b"declare" | b"abstract" | b"async" | b"public" | b"private" | b"protected" | b"static" | b"readonly" | b"override" | b"get" | b"set"
        ),
        Lang::Kotlin => matches!(
            w,
            b"fun" | b"class" | b"interface" | b"object" | b"val" | b"var" | b"typealias" | b"companion" | b"public" | b"private" | b"protected" | b"internal" | b"open" | b"abstract" | b"final" | b"sealed" | b"data" | b"enum" | b"annotation" | b"inner" | b"inline" | b"value" | b"suspend" | b"operator" | b"infix" | b"override" | b"external" | b"tailrec" | b"actual" | b"expect" | b"lateinit" | b"const" | b"vararg" | b"crossinline" | b"noinline" | b"context"
        ),
        _ => false,
    }
}

/// Could `line` be a definition? Looks at the first few words only (~50 ns):
/// definitions start with a keyword or a modifier; JS/TS methods start with
/// an identifier followed by `(` and need a `{` on the line; Python
/// constants start with an UPPER_CASE name.
fn may_define(lang: Lang, line: &[u8]) -> bool {
    let t = crate::trim_start(line);
    let mut pos = 0;
    for nth in 0..4 {
        let start = pos;
        while pos < t.len() && (is_word(t[pos]) || (t[pos] == b'!' && nth == 0)) {
            pos += 1;
        }
        let w = &t[start..pos];
        if w.is_empty() {
            return false;
        }
        if is_def_word(lang, w) {
            return true;
        }
        if nth == 0 {
            match lang {
                Lang::Python => {
                    if w[0].is_ascii_uppercase() && w.iter().all(|&b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') && w.len() >= 3 {
                        return true;
                    }
                    return false;
                }
                Lang::JavaScript | Lang::TypeScript => {
                    // method shape: name(...) ... {
                    let rest = crate::trim_start(&t[pos..]);
                    if rest.first().copied() == Some(b'(') || rest.first().copied() == Some(b'<') {
                        return memchr::memchr(b'{', rest).is_some();
                    }
                }
                _ => {}
            }
        }
        // modifiers may be followed by `(...)` (Kotlin context(...)) — skip a paren group
        while pos < t.len() && (t[pos] == b' ' || t[pos] == b'\t') {
            pos += 1;
        }
        if pos < t.len() && t[pos] == b'(' && lang == Lang::Kotlin
            && let Some(k) = memchr::memchr(b')', &t[pos..]) {
                pos += k + 1;
                while pos < t.len() && (t[pos] == b' ' || t[pos] == b'\t') {
                    pos += 1;
                }
            }
        if lang != Lang::JavaScript && lang != Lang::TypeScript && lang != Lang::Kotlin && lang != Lang::Rust && nth == 0 {
            return false;
        }
    }
    false
}

/// Cheap per-line check: if `line` starts a definition, return the name range
/// within the line. Used by scan mode before any file-level outline exists.
pub fn def_name_on_line(lang: Lang, line: &[u8]) -> Option<(usize, usize)> {
    let res = res_for(lang)?;
    if !may_define(lang, line) {
        return None;
    }
    let caps = res.re.captures(line)?;
    if caps.get(0)?.start() != 0 {
        return None;
    }
    let name = ["name", "name2", "name3", "name4", "name5", "name6", "name7", "name8", "name9", "name10"].iter().find_map(|g| caps.name(g));
    let m = match name {
        Some(n) => n,
        None => res.kinds.iter().find_map(|(g, _)| caps.name(g))?,
    };
    if let Some(mm) = caps.name("method") {
        let nm = &line[mm.start()..mm.end()];
        if JS_KEYWORDS.iter().any(|k| k.as_bytes() == nm) {
            return None;
        }
    }
    Some((m.start(), m.end()))
}

/// Extract definitions and compute enclosing ranges.
pub fn outline(lang: Lang, src: &[u8], lexed: &Lexed) -> Outline {
    let Some(res) = res_for(lang) else { return Outline::default() };
    let mut defs: Vec<Def> = Vec::new();
    let names = ["name", "name2", "name3", "name4", "name5", "name6", "name7", "name8", "name9", "name10"];
    let mut line_no = 0u32;
    let mut pos = 0usize;
    // Per-line matching after a keyword precheck is ~10× faster than
    // `captures_iter` over the whole buffer with a multi-line anchor.
    while pos < src.len() {
        line_no += 1;
        let le = memchr::memchr(b'\n', &src[pos..]).map(|k| pos + k).unwrap_or(src.len());
        let line = &src[pos..le];
        let ls = pos;
        pos = le + 1;
        if line.len() < 3 || !may_define(lang, line) {
            continue;
        }
        let Some(caps) = res.re.captures(line) else { continue };
        let m = caps.get(0).unwrap();
        if m.start() != 0 {
            continue;
        }
        // skip matches inside comments/strings (block comments spanning lines)
        if let Some(sp) = lexed.span_at(ls as u32)
            && (sp.kind != SpanKind::Docstring || lang != Lang::Rust) {
                continue;
            }
        let mut kind = None;
        for (g, k) in &res.kinds {
            if caps.name(g).is_some() {
                kind = Some(*k);
                break;
            }
        }
        let Some(kind) = kind else { continue };
        let name = names.iter().find_map(|g| caps.name(g));
        let (name_start, name_end) = match name {
            Some(n) => (n.start(), n.end()),
            None => {
                let g = res.kinds.iter().find_map(|(g, _)| caps.name(g)).unwrap();
                (g.start(), g.end())
            }
        };
        if kind == DefKind::Method {
            let nm = &line[name_start..name_end];
            if JS_KEYWORDS.iter().any(|k| k.as_bytes() == nm) {
                continue;
            }
        }
        let indent = crate::indent_of(line);
        if lang == Lang::Python && kind == DefKind::Constant && indent > 0 {
            continue;
        }
        defs.push(Def { name_start: (ls + name_start) as u32, name_end: (ls + name_end) as u32, start: ls as u32, end: 0, line: line_no, indent: indent.min(u16::MAX as usize) as u16, kind, parent: None });
    }
    // ends
    if lang.indent_scoped() {
        compute_ends_indent(src, &mut defs);
    } else {
        compute_ends_brace(src, lexed, &mut defs);
    }
    // parents by containment
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..defs.len() {
        while let Some(&top) = stack.last() {
            if defs[top].end <= defs[i].start {
                stack.pop();
            } else {
                break;
            }
        }
        defs[i].parent = stack.last().map(|&p| p as u32);
        stack.push(i);
    }
    // function inside a container => method
    for i in 0..defs.len() {
        if defs[i].kind == DefKind::Function
            && let Some(p) = defs[i].parent
                && defs[p as usize].kind.is_container() {
                    defs[i].kind = DefKind::Method;
                }
    }
    Outline { defs }
}

fn line_end(src: &[u8], off: usize) -> usize {
    memchr::memchr(b'\n', &src[off..]).map(|k| off + k + 1).unwrap_or(src.len())
}

fn compute_ends_indent(src: &[u8], defs: &mut [Def]) {
    for d in defs.iter_mut() {
        if d.kind == DefKind::Constant || d.kind == DefKind::Variable {
            d.end = line_end(src, d.start as usize) as u32;
            continue;
        }
        let mut pos = line_end(src, d.start as usize);
        let mut end = pos;
        while pos < src.len() {
            let le = line_end(src, pos);
            let line = &src[pos..le];
            let t = crate::trim_start(line);
            if t.is_empty() || t[0] == b'\n' || t[0] == b'\r' || t[0] == b'#' {
                pos = le;
                continue;
            }
            if crate::indent_of(line) <= d.indent as usize {
                break;
            }
            end = le;
            pos = le;
        }
        d.end = end as u32;
    }
}

fn compute_ends_brace(src: &[u8], lexed: &Lexed, defs: &mut [Def]) {
    let n = defs.len();
    for i in 0..n {
        let d = &defs[i];
        let start = d.start as usize;
        let le = line_end(src, start);
        if matches!(d.kind, DefKind::Constant | DefKind::Variable | DefKind::TypeAlias) {
            defs[i].end = le as u32;
            continue;
        }
        let limit = if i + 1 < n { (defs[i + 1].start as usize).min(start + 4096) } else { (start + 4096).min(src.len()) };
        // find first '{' or ';' outside noncode at paren depth 0 before `limit`
        let mut j = d.name_end as usize;
        let mut paren = 0i32;
        let mut body_open: Option<usize> = None;
        let mut terminated = false;
        while j < limit {
            let c = src[j];
            if let Some(sp) = lexed.span_at(j as u32) {
                j = sp.end as usize;
                continue;
            }
            match c {
                b'(' | b'[' => paren += 1,
                b')' | b']' => paren -= 1,
                b'{' if paren <= 0 => {
                    body_open = Some(j);
                    break;
                }
                b';' if paren <= 0 => {
                    terminated = true;
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        let end = match body_open {
            Some(open) => {
                // walk brace events from `open`
                let bi = lexed.braces.partition_point(|b| (b.off as usize) < open);
                let mut depth = 0i32;
                let mut e = src.len();
                for b in &lexed.braces[bi..] {
                    if b.open {
                        depth += 1;
                    } else {
                        depth -= 1;
                        if depth == 0 {
                            e = b.off as usize + 1;
                            break;
                        }
                    }
                }
                e
            }
            None => {
                if terminated { j + 1 } else { le.max(d.name_end as usize) }
            }
        };
        defs[i].end = end as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    fn names(lang: Lang, src: &[u8]) -> Vec<(String, &'static str, Option<u32>)> {
        let l = lex(lang, src);
        let o = outline(lang, src, &l);
        o.defs.iter().map(|d| (String::from_utf8_lossy(&src[d.name_start as usize..d.name_end as usize]).into_owned(), d.kind.name(), d.parent)).collect()
    }

    #[test]
    fn python_outline() {
        let src = b"MAX = 3\nclass A:\n    def m(self):\n        pass\n\n    x = 1\ndef f():\n    return 1\n";
        let v = names(Lang::Python, src);
        assert_eq!(v, vec![("MAX".into(), "const", None), ("A".into(), "class", None), ("m".into(), "method", Some(1)), ("f".into(), "fn", None)]);
    }

    #[test]
    fn rust_outline() {
        let src = b"pub struct S { a: u8 }\nimpl S {\n    pub fn new() -> S { S { a: 0 } }\n    fn helper(&self) {}\n}\nfn free() {}\ntrait T {\n    fn req(&self);\n}\n";
        let v = names(Lang::Rust, src);
        let n: Vec<_> = v.iter().map(|x| (x.0.as_str(), x.1, x.2)).collect();
        assert_eq!(n, vec![("S", "struct", None), ("S", "impl", None), ("new", "method", Some(1)), ("helper", "method", Some(1)), ("free", "fn", None), ("T", "trait", None), ("req", "method", Some(5))]);
    }

    #[test]
    fn ts_outline() {
        let src = b"export class C extends B {\n  private x = 1;\n  constructor() { super(); }\n  async run(a: number): Promise<void> {\n    if (a) { return; }\n  }\n}\nexport const go = async (x) => {\n  return x;\n};\nfunction plain() {}\n";
        let v = names(Lang::TypeScript, src);
        let n: Vec<_> = v.iter().map(|x| (x.0.as_str(), x.1, x.2)).collect();
        assert_eq!(n, vec![("C", "class", None), ("constructor", "method", Some(0)), ("run", "method", Some(0)), ("go", "fn", None), ("plain", "fn", None)]);
    }

    #[test]
    fn kotlin_outline() {
        let src = b"class Svc(val repo: Repo) {\n    suspend fun ApplicationCall.respond(msg: Any?) {\n        val x = 1\n    }\n    fun short() = 1\n    companion object {\n        const val K = 2\n    }\n}\nfun top() {}\n";
        let v = names(Lang::Kotlin, src);
        let n: Vec<_> = v.iter().map(|x| (x.0.as_str(), x.1, x.2)).collect();
        assert_eq!(n, vec![("Svc", "class", None), ("respond", "method", Some(0)), ("x", "var", Some(1)), ("short", "method", Some(0)), ("companion object", "object", Some(0)), ("K", "var", Some(4)), ("top", "fn", None)]);
    }
}

