//! Byte-level noncode lexer: finds comment and string spans and records brace
//! events outside them. One pass, no allocation per byte.

use crate::Lang;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    Comment,
    String,
    Docstring,
}

#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub kind: SpanKind,
}

#[derive(Clone, Copy, Debug)]
pub struct Brace {
    pub off: u32,
    pub open: bool,
}

#[derive(Debug, Default)]
pub struct Lexed {
    pub spans: Vec<Span>,
    pub braces: Vec<Brace>,
}

impl Lexed {
    /// Innermost noncode span containing `off`, if any.
    pub fn span_at(&self, off: u32) -> Option<&Span> {
        // spans are sorted and non-overlapping
        let i = self.spans.partition_point(|s| s.end <= off);
        self.spans.get(i).filter(|s| s.start <= off && off < s.end)
    }
}

/// Lex `src` for `lang`. Unknown languages produce no spans.
pub fn lex(lang: Lang, src: &[u8]) -> Lexed {
    match lang {
        Lang::Python => lex_python(src),
        Lang::Rust => lex_c_like(src, true, true, false, false),
        Lang::JavaScript | Lang::TypeScript => lex_c_like(src, false, false, true, false),
        Lang::Kotlin => lex_c_like(src, true, false, false, true),
        Lang::Extra(i) => match crate::extra::get(i) {
            Some(l) => lex_generic(src, l.line_comment.as_deref(), l.block_comment.as_ref().map(|(a, b)| (a.as_slice(), b.as_slice())), &l.strings),
            None => Lexed::default(),
        },
        _ => Lexed::default(),
    }
}

/// Comment and string spans for a runtime-loaded language from its spec:
/// one line-comment marker, one block-comment pair, string quote characters
/// with backslash escapes. Braces are recorded for block clipping.
pub fn lex_generic(src: &[u8], line: Option<&[u8]>, block: Option<(&[u8], &[u8])>, quotes: &[u8]) -> Lexed {
    let mut out = Lexed::default();
    let n = src.len();
    let mut i = 0;
    while i < n {
        let c = src[i];
        if let Some(lc) = line
            && !lc.is_empty()
            && src[i..].starts_with(lc)
        {
            let e = memchr::memchr(b'\n', &src[i..]).map(|k| i + k).unwrap_or(n);
            push_span(&mut out.spans, i, e, SpanKind::Comment);
            i = e;
            continue;
        }
        if let Some((open, close)) = block
            && !open.is_empty()
            && src[i..].starts_with(open)
        {
            let e = memchr::memmem::find(&src[i + open.len()..], close).map(|k| i + open.len() + k + close.len()).unwrap_or(n);
            push_span(&mut out.spans, i, e, SpanKind::Comment);
            i = e;
            continue;
        }
        if quotes.contains(&c) {
            let q = c;
            let mut j = i + 1;
            while j < n {
                let d = src[j];
                if d == b'\\' {
                    j += 2;
                    continue;
                }
                if d == q {
                    j += 1;
                    break;
                }
                if d == b'\n' && q != b'`' {
                    break; // unterminated: stop at end of line
                }
                j += 1;
            }
            let j = j.min(n);
            push_span(&mut out.spans, i, j, SpanKind::String);
            i = j.max(i + 1);
            continue;
        }
        if c == b'{' || c == b'}' {
            out.braces.push(Brace { off: i as u32, open: c == b'{' });
        }
        i += 1;
    }
    out
}

fn push_span(out: &mut Vec<Span>, start: usize, end: usize, kind: SpanKind) {
    if end > start {
        out.push(Span { start: start as u32, end: end as u32, kind });
    }
}

fn lex_python(src: &[u8]) -> Lexed {
    let mut out = Lexed::default();
    let n = src.len();
    let mut i = 0;
    // "docstring" = triple-quoted string that is the first token on its line
    while i < n {
        let c = src[i];
        match c {
            b'#' => {
                let e = memchr::memchr(b'\n', &src[i..]).map(|k| i + k).unwrap_or(n);
                push_span(&mut out.spans, i, e, SpanKind::Comment);
                i = e;
            }
            b'\'' | b'"' => {
                let q = c;
                let triple = i + 2 < n && src[i + 1] == q && src[i + 2] == q;
                let start = i;
                // string prefixes (r, b, f, u, rb, ...) are part of the token: extend start backwards
                let mut s = start;
                while s > 0 && src[s - 1].is_ascii_alphabetic() && s + 3 > start {
                    s -= 1;
                }
                let is_raw = src[s..start].iter().any(|&p| p == b'r' || p == b'R');
                let first_on_line = {
                    let ls = src[..s].iter().rposition(|&b| b == b'\n').map(|k| k + 1).unwrap_or(0);
                    src[ls..s].iter().all(|&b| b == b' ' || b == b'\t')
                };
                i += if triple { 3 } else { 1 };
                let end;
                loop {
                    if i >= n {
                        end = n;
                        break;
                    }
                    let d = src[i];
                    if d == b'\\' && !is_raw {
                        i += 2;
                        continue;
                    }
                    if triple {
                        if d == q && i + 2 < n && src[i + 1] == q && src[i + 2] == q {
                            end = i + 3;
                            break;
                        }
                        i += 1;
                    } else {
                        if d == q {
                            end = i + 1;
                            break;
                        }
                        if d == b'\n' {
                            end = i;
                            break;
                        }
                        i += 1;
                    }
                }
                let kind = if triple && first_on_line { SpanKind::Docstring } else { SpanKind::String };
                push_span(&mut out.spans, s, end, kind);
                i = end;
            }
            _ => i += 1,
        }
    }
    out.spans.sort_by_key(|s| s.start);
    out
}

/// C-like lexer: `//` and `/* */` comments (optionally nested), `"` strings,
/// optional raw strings (Rust `r#"..."#`, Kotlin `"""..."""`), optional
/// template literals (JS backticks), char literals handled conservatively.
fn lex_c_like(src: &[u8], nested_block: bool, rust: bool, js: bool, kotlin: bool) -> Lexed {
    let mut out = Lexed::default();
    let n = src.len();
    let mut i = 0;
    while i < n {
        let c = src[i];
        match c {
            b'/' if i + 1 < n && src[i + 1] == b'/' => {
                let e = memchr::memchr(b'\n', &src[i..]).map(|k| i + k).unwrap_or(n);
                // Rust doc comments are comments too (docstring kind for ///, //!)
                let kind = if rust && i + 2 < n && (src[i + 2] == b'/' || src[i + 2] == b'!') { SpanKind::Docstring } else { SpanKind::Comment };
                push_span(&mut out.spans, i, e, kind);
                i = e;
            }
            b'/' if i + 1 < n && src[i + 1] == b'*' => {
                let start = i;
                let doc = i + 2 < n && (src[i + 2] == b'*' || src[i + 2] == b'!') && !(i + 3 < n && src[i + 3] == b'/');
                let mut depth = 1;
                i += 2;
                while i < n {
                    if nested_block && i + 1 < n && src[i] == b'/' && src[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                        continue;
                    }
                    if i + 1 < n && src[i] == b'*' && src[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                        continue;
                    }
                    i += 1;
                }
                push_span(&mut out.spans, start, i.min(n), if doc { SpanKind::Docstring } else { SpanKind::Comment });
            }
            b'/' if js && regex_can_start(src, i) => {
                // regex literal: `/…/flags`, `/` inside `[…]` does not terminate
                let start = i;
                i += 1;
                let mut class = false;
                while i < n {
                    let d = src[i];
                    if d == b'\\' {
                        i += 2;
                        continue;
                    }
                    if d == b'\n' {
                        break;
                    }
                    if class {
                        if d == b']' {
                            class = false;
                        }
                    } else if d == b'[' {
                        class = true;
                    } else if d == b'/' {
                        i += 1;
                        while i < n && src[i].is_ascii_alphabetic() {
                            i += 1;
                        }
                        break;
                    }
                    i += 1;
                }
                push_span(&mut out.spans, start, i.min(n), SpanKind::String);
            }
            b'"' => {
                let start = i;
                if kotlin && i + 2 < n && src[i + 1] == b'"' && src[i + 2] == b'"' {
                    // raw string """..."""
                    i += 3;
                    while i < n {
                        if src[i] == b'"' && i + 2 < n && src[i + 1] == b'"' && src[i + 2] == b'"' {
                            i += 3;
                            // trailing quotes belong to the string ("""" edge case)
                            while i < n && src[i] == b'"' {
                                i += 1;
                            }
                            break;
                        }
                        i += 1;
                    }
                    push_span(&mut out.spans, start, i.min(n), SpanKind::String);
                    continue;
                }
                i += 1;
                let mut depth = 0i32; // Kotlin `${ … }` nesting
                while i < n {
                    let d = src[i];
                    if d == b'\\' {
                        i += 2;
                        continue;
                    }
                    if kotlin {
                        if d == b'$' && i + 1 < n && src[i + 1] == b'{' {
                            depth += 1;
                            i += 2;
                            continue;
                        }
                        if depth > 0 {
                            match d {
                                b'{' => depth += 1,
                                b'}' => depth -= 1,
                                b'"' => {
                                    // nested string inside the template expression: `"${m["k"]}"`
                                    i += 1;
                                    while i < n && src[i] != b'"' && src[i] != b'\n' {
                                        i += if src[i] == b'\\' { 2 } else { 1 };
                                    }
                                }
                                _ => {}
                            }
                            i += 1;
                            continue;
                        }
                    }
                    if d == b'"' {
                        i += 1;
                        break;
                    }
                    if d == b'\n' && !rust {
                        break; // unterminated on this line
                    }
                    i += 1;
                }
                push_span(&mut out.spans, start, i.min(n), SpanKind::String);
            }
            b'r' if rust && i + 1 < n && (src[i + 1] == b'"' || src[i + 1] == b'#') => {
                // raw string r"..." / r#"..."# / br"..." / cr#"..."#
                let prefixed = i > 0 && (src[i - 1] == b'b' || src[i - 1] == b'c') && (i < 2 || !is_ident_byte(src[i - 2]));
                let start = if prefixed { i - 1 } else { i };
                let mut j = i + 1;
                let mut hashes = 0;
                while j < n && src[j] == b'#' {
                    hashes += 1;
                    j += 1;
                }
                if j < n && src[j] == b'"' && (i == 0 || prefixed || !is_ident_byte(src[i - 1])) {
                    j += 1;
                    let mut end = n;
                    while j < n {
                        if src[j] == b'"' {
                            let mut k = j + 1;
                            let mut h = 0;
                            while k < n && src[k] == b'#' && h < hashes {
                                h += 1;
                                k += 1;
                            }
                            if h == hashes {
                                end = k;
                                break;
                            }
                        }
                        j += 1;
                    }
                    push_span(&mut out.spans, start, end, SpanKind::String);
                    i = end;
                } else {
                    i += 1;
                }
            }
            b'\'' => {
                // char literal or (Rust) lifetime / (JS,Kotlin) string. Conservative:
                // JS: full string; Rust/Kotlin: char literal only if it closes within 4 bytes.
                let start = i;
                if js {
                    i += 1;
                    while i < n {
                        let d = src[i];
                        if d == b'\\' {
                            i += 2;
                            continue;
                        }
                        if d == b'\'' {
                            i += 1;
                            break;
                        }
                        if d == b'\n' {
                            break;
                        }
                        i += 1;
                    }
                    push_span(&mut out.spans, start, i.min(n), SpanKind::String);
                } else {
                    let mut j = i + 1;
                    if j < n && src[j] == b'\\' {
                        j += 2;
                        while j < n && j < i + 8 && src[j] != b'\'' {
                            j += 1;
                        }
                    } else if j < n {
                        // one UTF-8 char
                        let w = utf8_width(src[j]);
                        j += w;
                    }
                    if j < n && src[j] == b'\'' {
                        push_span(&mut out.spans, start, j + 1, SpanKind::String);
                        i = j + 1;
                    } else {
                        i += 1;
                    }
                }
            }
            b'`' if js => {
                let start = i;
                i += 1;
                let mut depth = 0i32; // ${ } nesting (single level approximation)
                while i < n {
                    let d = src[i];
                    if d == b'\\' {
                        i += 2;
                        continue;
                    }
                    if d == b'$' && i + 1 < n && src[i + 1] == b'{' {
                        depth += 1;
                        i += 2;
                        continue;
                    }
                    if d == b'}' && depth > 0 {
                        depth -= 1;
                        i += 1;
                        continue;
                    }
                    if d == b'`' && depth == 0 {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                push_span(&mut out.spans, start, i.min(n), SpanKind::String);
            }
            b'{' => {
                out.braces.push(Brace { off: i as u32, open: true });
                i += 1;
            }
            b'}' => {
                out.braces.push(Brace { off: i as u32, open: false });
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// JS: can the `/` at `i` start a regex literal? Yes after an operator or
/// opening punctuation, at the start of a line, and after keywords that take
/// an expression (`return /x/`); no after a value (`a / b`, `f() / 2`).
fn regex_can_start(src: &[u8], i: usize) -> bool {
    let mut j = i;
    while j > 0 && (src[j - 1] == b' ' || src[j - 1] == b'\t') {
        j -= 1;
    }
    if j == 0 {
        return true;
    }
    let p = src[j - 1];
    if matches!(p, b'=' | b'(' | b',' | b':' | b'[' | b'!' | b'&' | b'|' | b'?' | b'{' | b'}' | b';' | b'\n' | b'\r' | b'+' | b'-' | b'*' | b'%' | b'<' | b'>' | b'~' | b'^') {
        return true;
    }
    if is_ident_byte(p) {
        let mut s = j - 1;
        while s > 0 && is_ident_byte(src[s - 1]) {
            s -= 1;
        }
        return matches!(&src[s..j], b"return" | b"typeof" | b"case" | b"in" | b"of" | b"delete" | b"void" | b"throw" | b"new" | b"do" | b"else" | b"instanceof" | b"yield" | b"await");
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn utf8_width(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_spans() {
        let src = b"x = 1  # c\ns = 'a\\'b'\ndef f():\n    \"\"\"doc\"\"\"\n    return f\"{x}\"\n";
        let l = lex(Lang::Python, src);
        let kinds: Vec<_> = l.spans.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, vec![SpanKind::Comment, SpanKind::String, SpanKind::Docstring, SpanKind::String]);
    }

    #[test]
    fn rust_spans_and_braces() {
        let src = b"/// d\nfn f<'a>(x: &'a str) -> char { let s = r#\"{\"#; '{' }\n";
        let l = lex(Lang::Rust, src);
        assert_eq!(l.spans.len(), 3, "{:?}", l.spans);
        assert_eq!(l.braces.len(), 2);
    }

    #[test]
    fn js_template() {
        let src = b"const s = `a ${ {b:1}.b } c`; // x\n";
        let l = lex(Lang::JavaScript, src);
        assert_eq!(l.spans.len(), 2);
        assert_eq!(l.braces.len(), 0);
    }

    #[test]
    fn rust_prefixed_raw_strings() {
        let src = b"let a = br\"x{\"; let b = cr#\"y{\"#; let c = b\"z\"; let d = xr(1);\n";
        let l = lex(Lang::Rust, src);
        let spans: Vec<(SpanKind, &str)> = l.spans.iter().map(|s| (s.kind, std::str::from_utf8(&src[s.start as usize..s.end as usize]).unwrap())).collect();
        assert_eq!(spans, vec![(SpanKind::String, "br\"x{\""), (SpanKind::String, "cr#\"y{\"#"), (SpanKind::String, "\"z\"")]);
        assert_eq!(l.braces.len(), 0);
    }

    #[test]
    fn kotlin_template_nested_quotes() {
        let src = b"val s = \"${m[\"k\"]} and ${f(\"{\")}\"\nfun f() { }\n";
        let l = lex(Lang::Kotlin, src);
        assert_eq!(l.spans.len(), 1, "{:?}", l.spans);
        assert_eq!(&src[l.spans[0].start as usize..l.spans[0].end as usize], &b"\"${m[\"k\"]} and ${f(\"{\")}\""[..]);
        assert_eq!(l.braces.len(), 2);
    }

    #[test]
    fn js_regex_literals() {
        let src = b"const re = /\"[^\"]*\"/g; // c\nif (x) { return /a\\/b{/.test(s); }\nconst r = a / b / c;\n[/x/, /y/i]\n";
        let l = lex(Lang::JavaScript, src);
        let kinds: Vec<(SpanKind, &str)> = l.spans.iter().map(|s| (s.kind, std::str::from_utf8(&src[s.start as usize..s.end as usize]).unwrap())).collect();
        assert_eq!(kinds, vec![(SpanKind::String, "/\"[^\"]*\"/g"), (SpanKind::Comment, "// c"), (SpanKind::String, "/a\\/b{/"), (SpanKind::String, "/x/"), (SpanKind::String, "/y/i")]);
        assert_eq!(l.braces.len(), 2, "braces inside the regex are not events");
    }

    #[test]
    fn kotlin_raw() {
        let src = b"val s = \"\"\"a \"b\" ${x}\"\"\"\nfun f() { }\n";
        let l = lex(Lang::Kotlin, src);
        assert_eq!(l.spans.len(), 1);
        assert_eq!(l.braces.len(), 2);
    }
}
