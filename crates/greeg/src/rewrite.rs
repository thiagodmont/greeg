//! Rewriting an `rg` command an agent runs into the equivalent `greeg`
//! command: shell words, ripgrep's flags, and what is left alone and why.
//! Plain std, so a fuzz target can include it as is.

/// Split a simple command into words (quotes and backslashes honoured).
/// `Err` names what the shell would expand or interpret that we do not model.
pub(crate) fn shell_words(s: &str) -> Result<Vec<String>, &'static str> {
    const UNTERMINATED: &str = "unterminated quote or escape";
    const CONTINUATION: &str = "line continuation";
    const EXPANSION: &str = "shell expansion";
    if s.contains('\0') {
        return Err("NUL byte");
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    let d = chars.next().ok_or(UNTERMINATED)?;
                    if d == '\'' {
                        break;
                    }
                    cur.push(d);
                }
            }
            '"' => {
                in_word = true;
                loop {
                    let d = chars.next().ok_or(UNTERMINATED)?;
                    match d {
                        '"' => break,
                        '\\' => {
                            let e = chars.next().ok_or(UNTERMINATED)?;
                            if e == '\n' {
                                return Err(CONTINUATION);
                            }
                            if !matches!(e, '"' | '\\' | '$' | '`') {
                                cur.push('\\');
                            }
                            cur.push(e);
                        }
                        '$' | '`' => return Err(EXPANSION),
                        _ => cur.push(d),
                    }
                }
            }
            '\\' => {
                in_word = true;
                let escaped = chars.next().ok_or(UNTERMINATED)?;
                if escaped == '\n' {
                    return Err(CONTINUATION);
                }
                cur.push(escaped);
            }
            ' ' | '\t' => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\n' if out.is_empty() && !in_word => {}
            '\n' if chars.clone().all(|c| matches!(c, ' ' | '\t' | '\n')) => break,
            '$' | '`' => return Err(EXPANSION),
            '<' | '>' => return Err("redirection"),
            ';' | '&' | '|' => return Err("pipeline, list or background job"),
            '\n' => return Err("several commands"),
            '(' | ')' | '{' | '}' => return Err("subshell, group or brace expansion"),
            '*' | '?' | '[' => return Err("unquoted glob"),
            '#' if !in_word => return Err("shell comment"),
            '~' if !in_word => return Err("tilde expansion"),
            _ => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    Ok(out)
}

pub(crate) fn quote(w: &str) -> String {
    if !w.is_empty()
        && w.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=,@+".contains(&b))
    {
        w.to_string()
    } else {
        format!("'{}'", w.replace('\'', "'\\''"))
    }
}

// ---------------------------------------------------------------------------
// flag tables
// ---------------------------------------------------------------------------

/// What to do with a flag. `Value` flags take the next word (or the attached
/// remainder of a short group); `Color` consumes and validates its value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flag {
    /// pass through unchanged
    Keep,
    /// cosmetic: drop
    Drop,
    /// color selection is replaced by ranked text
    Color,
    /// takes a value; emitted as `name value`
    Value,
    Fixed,
    Unrestricted,
    /// semantics greeg does not implement: do not rewrite
    Unsupported,
}

fn short_flag(c: char) -> Flag {
    use Flag::*;
    match c {
        'i' | 'w' | 'x' | 'l' | 'c' | 'S' | 's' | 'U' => Keep,
        'n' | 'N' | 'H' | 'p' => Drop,
        'F' => Fixed,
        'u' => Unrestricted,
        'A' | 'B' | 'C' | 'e' | 'g' | 't' | 'T' | 'j' | 'M' => Value,
        _ => Unsupported,
    }
}

fn long_flag(name: &str) -> Flag {
    use Flag::*;
    match name {
        "--ignore-case"
        | "--word-regexp"
        | "--line-regexp"
        | "--files-with-matches"
        | "--count"
        | "--smart-case"
        | "--case-sensitive"
        | "--multiline"
        | "--no-ignore"
        | "--hidden" => Keep,
        "--fixed-strings" => Fixed,
        "--unrestricted" => Unrestricted,
        "--line-number" | "--with-filename" | "--no-filename" | "--line-buffered"
        | "--no-line-number" | "--heading" | "--no-heading" | "--column" | "--no-column"
        | "--pretty" | "--trim" | "--block-buffered" | "--no-config" | "--mmap" | "--no-mmap" => {
            Drop
        }
        "--color" => Color,
        "--after-context" | "--before-context" | "--context" | "--regexp" | "--glob" | "--type"
        | "--type-not" | "--threads" | "--max-columns" | "--max-filesize" => Value,
        _ => Unsupported,
    }
}

// ---------------------------------------------------------------------------
// rewriting
// ---------------------------------------------------------------------------

/// Parsed rg invocation, ready to be re-emitted as `greeg`.
#[derive(Default)]
struct Parsed {
    flags: Vec<String>,
    /// patterns given with `-e`/`--regexp`
    patterns: Vec<String>,
    /// (word, seen after `--`)
    positional: Vec<(String, bool)>,
    fixed: bool,
    unrestricted: u8,
    max_filesize: Option<String>,
}

impl Parsed {
    /// Apply one flag; `name` is the canonical flag as it should be emitted
    /// (`-t`, `--type`, ...), `value` its value when `kind == Value`.
    fn apply(&mut self, kind: Flag, name: &str, value: Option<String>) -> Result<(), String> {
        let number = |v: &str| {
            v.parse::<u64>()
                .map(drop)
                .map_err(|_| format!("`{name}` needs a whole number"))
        };
        match kind {
            Flag::Keep => self.flags.push(name.to_string()),
            Flag::Drop => {}
            Flag::Color => {
                if !matches!(value.as_deref(), Some("never" | "always" | "auto" | "ansi")) {
                    return Err(format!("unknown `{name}` value"));
                }
            }
            Flag::Fixed => self.fixed = true,
            Flag::Unrestricted => self.unrestricted = self.unrestricted.saturating_add(1),
            Flag::Unsupported => return Err(format!("unsupported flag `{name}`")),
            Flag::Value => {
                let v = value.ok_or_else(|| format!("`{name}` needs a value"))?;
                match name {
                    "-e" | "--regexp" => self.patterns.push(v),
                    "--max-filesize" => {
                        number(&v)?;
                        if self.max_filesize.replace(v).is_some() {
                            return Err(format!("`{name}` given twice"));
                        }
                    }
                    "-A" | "-B" | "-C" | "-j" | "--after-context" | "--before-context"
                    | "--context" | "--threads" | "--max-columns" => {
                        number(&v)?;
                        self.flags.push(name.to_string());
                        self.flags.push(v);
                    }
                    "-M" => {
                        number(&v)?;
                        self.flags.push("--max-columns".into());
                        self.flags.push(v);
                    }
                    _ if v.starts_with('-') => self.flags.push(format!("{name}={v}")),
                    _ => {
                        self.flags.push(name.to_string());
                        self.flags.push(v);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Every subcommand name: a pattern spelled like one needs `-e`.
pub(crate) const VERBS: &[&str] = &[
    "def", "refs", "callers", "impls", "outline", "show", "map", "impact", "index", "doctor",
    "purge", "man", "hook", "lang", "budget", "stats",
];

/// The value word after a flag at `words[i - 1]`.
fn missing_value(words: &[String], i: usize, name: &str) -> Result<String, String> {
    words
        .get(i - 1)
        .cloned()
        .ok_or_else(|| format!("`{name}` needs a value"))
}

fn parse(words: &[String]) -> Result<Parsed, String> {
    let mut p = Parsed::default();
    let mut i = 1;
    let mut after_dd = false;
    while i < words.len() {
        let w = &words[i];
        i += 1;
        if after_dd || !w.starts_with('-') || w == "-" {
            p.positional.push((w.clone(), after_dd));
            continue;
        }
        if w == "--" {
            after_dd = true;
            continue;
        }
        if let Some(rest) = w.strip_prefix("--") {
            let (name, inline) = match rest.split_once('=') {
                Some((n, v)) => (format!("--{n}"), Some(v.to_string())),
                None => (w.clone(), None),
            };
            let kind = long_flag(&name);
            if kind == Flag::Unsupported {
                return Err(format!("unsupported flag `{name}`"));
            }
            if inline.is_some() && !matches!(kind, Flag::Value | Flag::Color) {
                return Err(format!("`{name}` does not take a value"));
            }
            let value = match kind {
                Flag::Value | Flag::Color => Some(match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        missing_value(words, i, &name)?
                    }
                }),
                _ => inline,
            };
            p.apply(kind, &name, value)?;
            continue;
        }
        // short flag group: `-in`, `-tjs`, `-A3`; stops at the first flag that takes a value
        let body = &w[1..];
        for (k, c) in body.char_indices() {
            let kind = short_flag(c);
            let name = format!("-{c}");
            if kind == Flag::Value {
                let rest = &body[k + c.len_utf8()..];
                let value = if rest.is_empty() {
                    i += 1;
                    missing_value(words, i, &name)?
                } else {
                    rest.to_string()
                };
                p.apply(kind, &name, Some(value))?;
                break;
            }
            p.apply(kind, &name, None)?;
        }
    }
    Ok(p)
}

/// Why a program other than `rg` is left alone.
fn other_program(first: &str) -> &'static str {
    match first {
        "grep" | "egrep" | "fgrep" => {
            "grep traversal, regex dialect and binary handling differ from greeg"
        }
        _ if first.ends_with("/rg") => "explicit executable path (only `rg` is rewritten)",
        _ if first.contains('=') => "environment assignment",
        _ => "not an rg command",
    }
}

/// Rewrite a simple rg command to argv: `(original words, greeg words)`;
/// `Err` = leave it alone, and why.
fn rewrite_words(seg: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let words = shell_words(seg)?;
    let first = words.first().ok_or("empty command")?;
    if first != "rg" {
        return Err(other_program(first).into());
    }
    let mut p = parse(&words)?;
    if p.patterns.len() > 1 {
        return Err("several -e patterns (greeg takes one)".into());
    }
    let (pattern, pattern_dd) = match p.patterns.pop() {
        Some(e) => (e, false),
        None => {
            if p.positional.is_empty() {
                return Err("no pattern".into());
            }
            p.positional.remove(0)
        }
    };
    let paths = std::mem::take(&mut p.positional);
    // stdin readers become tree searches: leave them alone
    if paths.iter().any(|(w, _)| w == "-") {
        return Err("stdin search".into());
    }

    let mut out: Vec<String> = vec!["greeg".into(), "--matching".into(), "exact".into()];
    out.extend([
        "--max-filesize".into(),
        p.max_filesize.unwrap_or_else(|| u64::MAX.to_string()),
    ]);
    out.extend(p.flags.clone());
    match p.unrestricted {
        0 => {}
        1 => out.push("--no-ignore".into()),
        2 => out.extend(["--no-ignore".to_string(), "--hidden".to_string()]),
        _ => return Err("-uuu also searches binary files".into()),
    }
    if p.fixed {
        out.push("-F".into());
    }
    let pattern_dd = pattern_dd || pattern.starts_with('-');
    if !pattern_dd && VERBS.contains(&pattern.as_str()) {
        out.push("-e".into());
    }
    let mut dd_done = false;
    for (w, dd) in std::iter::once((pattern, pattern_dd)).chain(paths) {
        if dd && !dd_done {
            out.push("--".into());
            dd_done = true;
        }
        out.push(w);
    }
    Ok((words, out))
}

/// A command line split at its top-level `;`, `&&`, `||` and newlines: each
/// segment's byte range. `Err` names shell syntax this does not model, so a
/// split could be wrong: pipes, background jobs, subshells and groups,
/// substitutions, here-documents, comments and line continuations.
pub(crate) fn list_segments(s: &str) -> Result<Vec<std::ops::Range<usize>>, &'static str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let (mut single, mut double, mut escaped) = (false, false, false);
    // the next byte starts a word: after a separator or an unescaped blank
    let mut word_start = true;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        let at_word = std::mem::replace(&mut word_start, false);
        if single {
            single = c != b'\'';
        } else if escaped {
            if c == b'\n' {
                return Err("line continuation");
            }
            escaped = false;
        } else if c == b'\\' {
            escaped = true;
        } else if double {
            match c {
                b'"' => double = false,
                b'`' => return Err("command substitution"),
                b'$' if next == Some(b'(') => return Err("command substitution"),
                _ => {}
            }
        } else {
            let mut sep = 0;
            match c {
                b' ' | b'\t' => word_start = true,
                b'\'' => single = true,
                b'"' => double = true,
                b'`' => return Err("command substitution"),
                b'$' if matches!(next, Some(b'(' | b'{')) => return Err("command substitution"),
                // `$'…'` takes backslash escapes, so `\'` would not end it here
                b'$' if matches!(next, Some(b'\'' | b'"')) => {
                    return Err("ANSI-C or locale quoting");
                }
                b'(' | b')' | b'{' | b'}' => return Err("subshell, group or brace expansion"),
                // `[[ a && b ]]` holds operators that are not list separators
                b'[' if at_word && next == Some(b'[') => return Err("conditional expression"),
                b'<' if next == Some(b'<') => return Err("here-document"),
                b'#' if at_word => return Err("shell comment"),
                b';' if next == Some(b';') => return Err("case syntax"),
                b';' | b'\n' => sep = 1,
                b'&' if next == Some(b'&') => sep = 2,
                b'|' if next == Some(b'|') => sep = 2,
                b'|' => return Err("pipeline"),
                // `2>&1`, `>&2` and `&>` redirect; any other `&` is a background job
                b'&' if next == Some(b'>') || (i > 0 && matches!(b[i - 1], b'>' | b'<')) => {}
                b'&' => return Err("background job"),
                _ => {}
            }
            if sep > 0 {
                out.push(start..i);
                i += sep;
                start = i;
                word_start = true;
                continue;
            }
        }
        i += 1;
    }
    if single || double || escaped {
        return Err("unterminated quote or escape");
    }
    out.push(start..b.len());
    Ok(out)
}

/// `seg` without the blanks around it (spaces and tabs, which the shell
/// splits on; an escaped one belongs to the last word): (offset, body).
pub(crate) fn trim_blanks(seg: &str) -> (usize, &str) {
    let b = seg.as_bytes();
    let lead = b.iter().take_while(|&&c| matches!(c, b' ' | b'\t')).count();
    let mut end = b.len();
    while end > lead && matches!(b[end - 1], b' ' | b'\t') {
        let backslashes = b[lead..end - 1]
            .iter()
            .rev()
            .take_while(|&&c| c == b'\\')
            .count();
        if backslashes % 2 == 1 {
            break;
        }
        end -= 1;
    }
    (lead, &seg[lead..end])
}

/// A command that can change what a later command in its line sees: the
/// environment (`RIPGREP_CONFIG_PATH`, for one), aliases or shell options.
fn changes_environment(seg: &str) -> bool {
    let first = seg.split([' ', '\t']).next().unwrap_or("");
    let assignment = first.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
            && !name.as_bytes()[0].is_ascii_digit()
    });
    assignment
        || matches!(
            first,
            "export"
                | "source"
                | "."
                | "eval"
                | "set"
                | "unset"
                | "alias"
                | "declare"
                | "typeset"
                | "local"
                | "readonly"
                | "env"
                | "shopt"
                | "enable"
                | "hash"
        )
}

/// One rewritten command of a Bash command line: its words before and after
/// (for the stats records).
pub struct Part {
    pub original: Vec<String>,
    pub rewritten: Vec<String>,
    /// The rewritten command as it appears in the line.
    pub command: String,
    /// Another command runs before it in the line (a `cd`, for one), so its
    /// stats record could not be replayed as it ran.
    pub after_other: bool,
}

/// A rewritten Bash command line.
pub struct Rewrite {
    pub command: String,
    pub parts: Vec<Part>,
    /// Every command in the line is one the hook rewrote.
    pub whole: bool,
}

fn part(seg: &str) -> Result<Part, String> {
    let (original, rewritten) = rewrite_words(seg)?;
    let command = rewritten
        .iter()
        .map(|w| quote(w))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(Part {
        original,
        rewritten,
        command,
        after_other: false,
    })
}

/// A simple rg command, or each rg command of a `;`/`&&`/`||` list, becomes
/// greeg; every other byte of the line is kept. Exit statuses agree (0 found,
/// 1 not, 2 error), so the list's control flow is unchanged. Pipelines and
/// other compound syntax are declined: a consumer may expect rg's output.
pub fn rewrite_full(cmd: &str) -> Result<Rewrite, String> {
    let segments = list_segments(cmd)?;
    let mut command = String::with_capacity(cmd.len() + 64);
    let mut parts: Vec<Part> = Vec::new();
    let mut why = None;
    let mut env = false;
    let mut at = 0;
    for r in segments {
        let (lead, body) = trim_blanks(&cmd[r.clone()]);
        if body.is_empty() {
            continue;
        }
        match part(body) {
            Ok(mut p) => {
                p.after_other = why.is_some();
                command.push_str(&cmd[at..r.start + lead]);
                command.push_str(&p.command);
                at = r.start + lead + body.len();
                parts.push(p);
            }
            // left as it is; an rg command's reason says more than another's
            Err(e) => {
                env |= changes_environment(body);
                let rg = shell_words(body).is_ok_and(|w| w.first().is_some_and(|f| f == "rg"));
                if rg && !why.as_ref().is_some_and(|(was_rg, _)| *was_rg) || why.is_none() {
                    why = Some((rg, e));
                }
            }
        }
    }
    if parts.is_empty() {
        return Err(why.map_or_else(|| "not an rg command".into(), |(_, e)| e));
    }
    if env {
        return Err("another command in the line can change the environment".into());
    }
    command.push_str(&cmd[at..]);
    let whole = why.is_none();
    Ok(Rewrite {
        command,
        parts,
        whole,
    })
}
