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

/// A rewritten Bash command and what changed in it (for the stats records).
pub struct Rewrite {
    pub command: String,
    pub original: Vec<String>,
    pub rewritten: Vec<String>,
}

/// Only complete simple commands qualify; a shell tail may consume another
/// output dialect or change permission and exit-status behavior.
pub fn rewrite_full(cmd: &str) -> Result<Rewrite, String> {
    let (original, rewritten) = rewrite_words(cmd)?;
    let command = rewritten
        .iter()
        .map(|w| quote(w))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(Rewrite {
        command,
        original,
        rewritten,
    })
}
