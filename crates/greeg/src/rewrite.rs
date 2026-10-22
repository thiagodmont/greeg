//! Rewriting an `rg` command an agent runs into the equivalent `greeg`
//! command: shell words, ripgrep's flags, and what is left alone and why.
//! Plain std, so a fuzz target can include it as is.

/// Split a simple command into words (quotes and backslashes honoured).
/// `Err` names what the shell would expand or interpret that we do not model.
pub(crate) fn shell_words(s: &str) -> Result<Vec<String>, &'static str> {
    split_words(s).map(|(words, _)| words)
}

/// [`shell_words`], and whether a word is an unquoted glob after
/// `--include=`, `--exclude=` or `--exclude-dir=`: the shell leaves it as it
/// is unless a file in the working directory has a name it matches.
fn split_words(s: &str) -> Result<(Vec<String>, bool), &'static str> {
    const UNTERMINATED: &str = "unterminated quote or escape";
    const CONTINUATION: &str = "line continuation";
    const EXPANSION: &str = "shell expansion";
    if s.contains('\0') {
        return Err("NUL byte");
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut glob = false;
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
            // a word starting `--include=` only matches a file named so: none,
            // so the shell passes grep's glob on as it is
            '*' | '?' | '['
                if ["--include=", "--exclude=", "--exclude-dir="]
                    .iter()
                    .any(|f| cur.starts_with(f)) =>
            {
                glob = true;
                cur.push(c)
            }
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
    Ok((out, glob))
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
    "def", "refs", "callers", "impls", "outline", "show", "map", "impact", "find", "index",
    "doctor", "purge", "man", "hook", "lang", "budget", "stats",
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

/// Why a program other than `rg` or `grep` is left alone.
fn other_program(first: &str) -> &'static str {
    match first {
        _ if first.ends_with("/rg") || first.ends_with("grep") && first.contains('/') => {
            "explicit executable path (only `rg` and `grep` are rewritten)"
        }
        _ if first.contains('=') => "environment assignment",
        _ => "not an rg or grep command",
    }
}

/// Rewrite a simple rg or recursive grep command to argv: `(original words,
/// greeg words)`; `Err` = leave it alone, and why.
fn rewrite_words(seg: &str) -> Result<Part, String> {
    let (words, unquoted_glob) = split_words(seg)?;
    let first = words.first().ok_or("empty command")?;
    let (rewritten, regex, reader) = match first.as_str() {
        "rg" => (
            rewrite_rg(&words)?,
            None,
            Err("only grep -r's lines go to another reader".into()),
        ),
        "grep" | "egrep" | "fgrep" => rewrite_grep(&words)?,
        _ => return Err(other_program(first).into()),
    };
    Ok(Part {
        original: words,
        command: command_of(&rewritten),
        rewritten,
        after_other: false,
        piped: false,
        unquoted_glob,
        regex,
        reader,
        lone_operand: None,
        cds: Some(Vec::new()),
    })
}

fn command_of(words: &[String]) -> String {
    words.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ")
}

/// A recursive grep's lines for another reader than `head`: every match, in
/// grep's format. `p.reader` says whether greeg's lines are grep's here.
fn into_reader(mut p: Part, redirections: &str) -> Result<Part, String> {
    let lone = std::mem::replace(&mut p.reader, Ok(None))?;
    if redirections.contains("2>&1") {
        return Err("`2>&1` would send greeg's notes to the reader".into());
    }
    // after `greeg --matching exact`
    p.rewritten
        .splice(3..3, ["--budget".to_string(), "0".to_string()]);
    p.command = command_of(&p.rewritten);
    p.piped = true;
    p.lone_operand = lone;
    Ok(p)
}

/// greeg prints the paths under `operand` as grep does: as written, with no
/// `./` or `//` to drop.
fn printed_as_written(operand: &str) -> bool {
    operand != "."
        && !operand.starts_with("./")
        && !operand.ends_with("/.")
        && !operand.contains("/./")
        && !operand.contains("//")
}

/// `greeg --matching exact` with no file-size limit, the given flags, then the
/// pattern and paths (`-e` before a pattern spelled like a subcommand, `--`
/// before words that must not read as flags).
fn greeg_words(
    flags: Vec<String>,
    pattern: (String, bool),
    paths: Vec<(String, bool)>,
    max_filesize: Option<String>,
) -> Result<Vec<String>, String> {
    // stdin readers become tree searches: leave them alone
    if paths.iter().any(|(w, _)| w == "-") {
        return Err("stdin search".into());
    }
    let mut out: Vec<String> = vec!["greeg".into(), "--matching".into(), "exact".into()];
    out.extend([
        "--max-filesize".into(),
        max_filesize.unwrap_or_else(|| u64::MAX.to_string()),
    ]);
    out.extend(flags);
    let (pattern, pattern_dd) = pattern;
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
    Ok(out)
}

fn rewrite_rg(words: &[String]) -> Result<Vec<String>, String> {
    let mut p = parse(words)?;
    if p.patterns.len() > 1 {
        return Err("several -e patterns (greeg takes one)".into());
    }
    let pattern = match p.patterns.pop() {
        Some(e) => (e, false),
        None => {
            if p.positional.is_empty() {
                return Err("no pattern".into());
            }
            p.positional.remove(0)
        }
    };
    let mut flags = std::mem::take(&mut p.flags);
    match p.unrestricted {
        0 => {}
        1 => flags.push("--no-ignore".into()),
        2 => flags.extend(["--no-ignore".to_string(), "--hidden".to_string()]),
        _ => return Err("-uuu also searches binary files".into()),
    }
    if p.fixed {
        flags.push("-F".into());
    }
    greeg_words(
        flags,
        pattern,
        std::mem::take(&mut p.positional),
        p.max_filesize,
    )
}

/// A recursive grep (`grep -r`, with GNU's long flags) as greeg: the pattern
/// translated from POSIX syntax, `--include`/`--exclude`/`--exclude-dir` as
/// globs. Ignore rules apply, as in rg; greeg says so on stderr
/// (`--from-grep`). Flags with no exact greeg meaning decline.
fn rewrite_grep(words: &[String]) -> Result<GrepRewrite, String> {
    // grep refuses two different matchers (`-E -F`, `egrep -F`)
    let mut matcher = match words[0].as_str() {
        "egrep" => Some('E'),
        "fgrep" => Some('F'),
        _ => None,
    };
    let mut set_matcher = |m: char| match matcher {
        Some(was) if was != m => Err("conflicting matchers (-E, -F, -G)".to_string()),
        _ => {
            matcher = Some(m);
            Ok(())
        }
    };
    let mut recursive = false;
    let (mut numbered, mut colored) = (false, false);
    let mut flags: Vec<String> = Vec::new();
    let (mut include, mut exclude) = (Vec::new(), Vec::new());
    let mut patterns = Vec::new();
    let mut positional = Vec::new();
    let mut after_dd = false;
    let number = |name: &str, v: &str| match v.parse::<u64>() {
        Ok(_) => Ok(()),
        Err(_) => Err(format!("`{name}` needs a whole number")),
    };
    let file_glob = |name: &str, g: &str| {
        // grep matches these against file names only; braces are literal there
        if g.is_empty() || g.contains(['/', '{', '}']) {
            Err(format!("`{name}={g}` has no glob equivalent"))
        } else {
            Ok(g.to_string())
        }
    };
    let mut i = 1;
    while i < words.len() {
        let w = &words[i];
        i += 1;
        if after_dd || !w.starts_with('-') || w == "-" {
            positional.push((w.clone(), after_dd));
            continue;
        }
        if w == "--" {
            after_dd = true;
            continue;
        }
        if let Some(rest) = w.strip_prefix("--") {
            let (name, inline) = match rest.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (rest, None),
            };
            let flag = format!("--{name}");
            let takes_value = matches!(
                name,
                "regexp"
                    | "include"
                    | "exclude"
                    | "exclude-dir"
                    | "after-context"
                    | "before-context"
                    | "context"
                    | "binary-files"
            );
            let value = if takes_value {
                Some(match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        missing_value(words, i, &flag)?
                    }
                })
            } else {
                if inline.is_some() && !matches!(name, "color" | "colour") {
                    return Err(format!("`{flag}` does not take a value"));
                }
                inline
            };
            let v = value.unwrap_or_default();
            match name {
                "recursive" => recursive = true,
                "dereference-recursive" => return Err("grep -R follows symlinks".into()),
                "line-number" => numbered = true,
                "with-filename" => {}
                "color" | "colour" if matches!(v.as_str(), "" | "auto" | "never") => {}
                "color" | "colour" if v == "always" => colored = true,
                "color" | "colour" => return Err(format!("`{flag}={v}` is not a color mode")),
                "no-messages" => {
                    return Err("grep -s would hide greeg's note on the files it skips".into());
                }
                "binary-files" if v == "without-match" => {}
                "ignore-case" => flags.push("-i".into()),
                "word-regexp" => flags.push("-w".into()),
                "line-regexp" => flags.push("-x".into()),
                "files-with-matches" => flags.push("-l".into()),
                "extended-regexp" => set_matcher('E')?,
                "basic-regexp" => set_matcher('G')?,
                "fixed-strings" => set_matcher('F')?,
                "regexp" => patterns.push(v),
                "include" => include.push(file_glob(&flag, &v)?),
                "exclude" => exclude.push(format!("!{}", file_glob(&flag, &v)?)),
                "exclude-dir" => exclude.push(format!("!{}/", file_glob(&flag, &v)?)),
                "after-context" | "before-context" | "context" => {
                    number(&flag, &v)?;
                    flags.extend([flag, v]);
                }
                _ => return Err(format!("unsupported grep flag `{flag}`")),
            }
            continue;
        }
        let body = &w[1..];
        for (k, c) in body.char_indices() {
            match c {
                'r' => recursive = true,
                'R' => return Err("grep -R follows symlinks".into()),
                'n' => numbered = true,
                'H' | 'I' => {}
                's' => return Err("grep -s would hide greeg's note on the files it skips".into()),
                'i' | 'w' | 'x' | 'l' => flags.push(format!("-{c}")),
                'E' | 'G' | 'F' => set_matcher(c)?,
                'c' => return Err("grep -c also counts files without a match".into()),
                'h' => return Err("grep -h leaves out file names".into()),
                'e' | 'A' | 'B' | 'C' => {
                    let rest = &body[k + 1..];
                    let v = if rest.is_empty() {
                        i += 1;
                        missing_value(words, i, &format!("-{c}"))?
                    } else {
                        rest.to_string()
                    };
                    if c == 'e' {
                        patterns.push(v);
                    } else {
                        number(&format!("-{c}"), &v)?;
                        flags.extend([format!("-{c}"), v]);
                    }
                    break;
                }
                _ => return Err(format!("unsupported grep flag `-{c}`")),
            }
        }
    }
    if !recursive {
        return Err("grep without -r searches the files it names".into());
    }
    if patterns.len() > 1 {
        return Err("several -e patterns (greeg takes one)".into());
    }
    let (pattern, dd) = match patterns.pop() {
        Some(e) => (e, false),
        None => {
            if positional.is_empty() {
                return Err("no pattern".into());
            }
            positional.remove(0)
        }
    };
    let listed = flags.iter().any(|f| f == "-l");
    let reader = if !numbered && !listed {
        Err("greeg numbers the lines grep prints without -n".to_string())
    } else if colored {
        Err("`--color=always` colors grep's lines".to_string())
    } else if positional.is_empty() {
        Err("grep -r with no path prints `./` before its paths on some systems".to_string())
    } else if let Some((w, _)) = positional.iter().find(|(w, _)| !printed_as_written(w)) {
        Err(format!(
            "greeg prints the paths under `{w}` without its `./` or `//`"
        ))
    } else {
        Ok((positional.len() == 1).then(|| positional[0].0.clone()))
    };
    let (extended, fixed) = (matcher == Some('E'), matcher == Some('F'));
    let regex = (!fixed)
        .then(|| posix_regex(&pattern, extended))
        .transpose()?;
    // with no file operand grep -r searches the working directory
    if positional.is_empty() {
        positional.push((".".to_string(), false));
    }
    let pattern = match &regex {
        Some(r) => r.clone(),
        None => {
            flags.push("-F".into());
            pattern
        }
    };
    let mut all = vec!["--from-grep".to_string()];
    all.extend(flags);
    // grep's exclusions win whatever their order; a later glob wins in greeg
    for g in include.into_iter().chain(exclude) {
        all.extend(["-g".to_string(), g]);
    }
    Ok((
        greeg_words(all, (pattern, dd), positional, None)?,
        regex,
        reader,
    ))
}

/// A recursive grep's greeg words, its translated regex, and whether its
/// lines can go to another reader (with the lone operand to check).
type GrepRewrite = (Vec<String>, Option<String>, Result<Option<String>, String>);

/// A POSIX basic (grep's default) or extended (`-E`) regular expression as
/// the Rust regex that matches the same lines. GNU's extensions are kept
/// (`\|`, `\+`, `\?` in basic; `\<`, `\>`, `\b`, `\w`, `\s`). `Err` for what
/// has no exact equivalent: back-references and other escapes.
pub(crate) fn posix_regex(p: &str, extended: bool) -> Result<String, String> {
    let c: Vec<char> = p.chars().collect();
    let mut out = String::with_capacity(p.len() + 8);
    // where `*` is literal: the start of the expression, a group or an alternative
    let mut at_start = true;
    let mut depth = 0usize;
    let mut i = 0;
    let interval = |c: &[char], from: usize, close: &str| -> Option<(String, usize)> {
        // `m`, `m,`, `m,n` or `,n`, then `close`
        let mut j = from;
        let mut body = String::new();
        while j < c.len() && (c[j].is_ascii_digit() || c[j] == ',') {
            body.push(c[j]);
            j += 1;
        }
        let rest: String = c[j..].iter().take(close.chars().count()).collect();
        let ok = !body.is_empty() && body.matches(',').count() <= 1 && body != "," && rest == close;
        ok.then(|| (body, j + close.chars().count()))
    };
    while i < c.len() {
        let ch = c[i];
        let start = std::mem::replace(&mut at_start, false);
        i += 1;
        match ch {
            '[' => i = bracket(&c, i, &mut out)?,
            '\\' => {
                let e = *c.get(i).ok_or("a trailing backslash")?;
                i += 1;
                match e {
                    '(' if !extended => {
                        out.push('(');
                        depth += 1;
                        at_start = true;
                    }
                    ')' if !extended => {
                        depth = depth.checked_sub(1).ok_or("an unmatched \\)")?;
                        out.push(')');
                    }
                    '|' if !extended => {
                        out.push('|');
                        at_start = true;
                    }
                    '+' | '?' if !extended && !start => out.push(e),
                    '{' if !extended => {
                        let (body, next) =
                            interval(&c, i, "\\}").ok_or("a malformed \\{interval\\}")?;
                        if start {
                            return Err("an interval with nothing to repeat".into());
                        }
                        out.push('{');
                        out.push_str(if body.starts_with(',') { "0" } else { "" });
                        out.push_str(&body);
                        out.push('}');
                        i = next;
                    }
                    '<' | '>' | 'b' | 'B' | 'w' | 'W' | 's' | 'S' => {
                        out.push('\\');
                        out.push(e);
                    }
                    '1'..='9' => return Err("a back-reference".into()),
                    _ if e.is_ascii_punctuation() => {
                        out.push('\\');
                        out.push(e);
                    }
                    _ => return Err(format!("`\\{e}` is not grep syntax")),
                }
            }
            '*' if start => out.push_str("\\*"),
            '*' | '.' => out.push(ch),
            '^' if extended || start => {
                out.push('^');
                at_start = true;
            }
            '^' => out.push_str("\\^"),
            '$' => {
                let ends = extended
                    || i == c.len()
                    || (c[i] == '\\' && matches!(c.get(i + 1), Some(')' | '|')));
                out.push_str(if ends { "$" } else { "\\$" });
            }
            '+' | '?' if extended && !start => out.push(ch),
            '{' if extended && !start => match interval(&c, i, "}") {
                Some((body, next)) => {
                    out.push('{');
                    out.push_str(if body.starts_with(',') { "0" } else { "" });
                    out.push_str(&body);
                    out.push('}');
                    i = next;
                }
                None => out.push_str("\\{"),
            },
            '(' if extended => {
                out.push('(');
                depth += 1;
                at_start = true;
            }
            ')' if extended && depth > 0 => {
                out.push(')');
                depth -= 1;
            }
            '|' if extended => {
                out.push('|');
                at_start = true;
            }
            '+' | '?' | '{' | '}' | '(' | ')' | '|' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    if depth > 0 {
        return Err("an unmatched (".into());
    }
    Ok(out)
}

/// The bracket expression after `[` at `c[i..]`, written to `out` as a Rust
/// class; returns the index after its `]`.
fn bracket(c: &[char], mut i: usize, out: &mut String) -> Result<usize, String> {
    const CLASSES: &[&str] = &[
        "alnum", "alpha", "blank", "cntrl", "digit", "graph", "lower", "print", "punct", "space",
        "upper", "xdigit",
    ];
    out.push('[');
    if c.get(i) == Some(&'^') {
        out.push('^');
        i += 1;
    }
    // a `]` first is a member
    if c.get(i) == Some(&']') {
        out.push_str("\\]");
        i += 1;
    }
    loop {
        let ch = *c.get(i).ok_or("an unterminated bracket expression")?;
        i += 1;
        match ch {
            ']' => {
                out.push(']');
                return Ok(i);
            }
            '[' if c.get(i) == Some(&':') => {
                let name: String = c[i + 1..].iter().take_while(|&&x| x != ':').collect();
                let close = i + 1 + name.chars().count();
                if !CLASSES.contains(&name.as_str()) || c.get(close + 1) != Some(&']') {
                    return Err(format!("`[:{name}` is not a character class"));
                }
                out.push_str(&format!("[:{name}:]"));
                i = close + 2;
            }
            '[' if matches!(c.get(i), Some('=' | '.')) => {
                return Err("a collating element in a bracket expression".into());
            }
            // literal in POSIX, syntax in a Rust class
            '[' | '\\' | '&' | '~' => {
                out.push('\\');
                out.push(ch);
            }
            '-' if c.get(i) == Some(&'-') => return Err("`--` in a bracket expression".into()),
            _ => out.push(ch),
        }
    }
}

/// A command of a list: the byte ranges of its pipeline stages.
pub(crate) struct Segment {
    pub stages: Vec<std::ops::Range<usize>>,
}

/// A command line split at its top-level `;`, `&&`, `||` and newlines, each
/// command at its pipes. `Err` names shell syntax this does not model, so a
/// split could be wrong: background jobs, subshells and groups, substitutions,
/// here-documents, comments and line continuations.
pub(crate) fn list_segments(s: &str) -> Result<Vec<Segment>, &'static str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut stage = 0;
    let mut stages = Vec::new();
    let (mut single, mut double, mut escaped) = (false, false, false);
    // the next byte starts a word: after a separator or an unescaped blank
    let mut word_start = true;
    // a `|` with no command after it yet: a newline would continue the pipe
    let mut piping = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        let at_word = std::mem::replace(&mut word_start, false);
        if !matches!(c, b' ' | b'\t' | b'\n') {
            piping = false;
        }
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
                b'\n' if piping => return Err("pipeline across lines"),
                b';' | b'\n' => sep = 1,
                b'&' if next == Some(b'&') => sep = 2,
                b'|' if next == Some(b'|') => sep = 2,
                b'|' if next == Some(b'&') => return Err("pipeline of stderr"),
                b'|' => {
                    stages.push(stage..i);
                    stage = i + 1;
                    word_start = true;
                    piping = true;
                }
                // `2>&1`, `>&2` and `&>` redirect; any other `&` is a background job
                b'&' if next == Some(b'>') || (i > 0 && matches!(b[i - 1], b'>' | b'<')) => {}
                b'&' => return Err("background job"),
                _ => {}
            }
            if sep > 0 {
                stages.push(stage..i);
                out.push(Segment {
                    stages: std::mem::take(&mut stages),
                });
                i += sep;
                stage = i;
                word_start = true;
                continue;
            }
        }
        i += 1;
    }
    if single || double || escaped {
        return Err("unterminated quote or escape");
    }
    stages.push(stage..b.len());
    out.push(Segment { stages });
    Ok(out)
}

/// `head`, `head -N`, `head -n N`, `head -nN` or `head --lines=N`: it prints
/// the first lines it reads and touches nothing else.
fn is_head(stage: &str) -> bool {
    let Ok(w) = shell_words(stage) else {
        return false;
    };
    let count = |n: &str| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit());
    match w.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["head"] => true,
        ["head", "-n", n] => count(n),
        ["head", f] => [
            f.strip_prefix("--lines="),
            f.strip_prefix("-n"),
            f.strip_prefix('-'),
        ]
        .into_iter()
        .flatten()
        .any(count),
        _ => false,
    }
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

/// `seg` split before its trailing redirections that leave its output alone:
/// stderr to `/dev/null` or stdout (`2>/dev/null`, `2>&1`) and stdin from
/// `/dev/null`. The redirections are kept as they are.
fn split_redirections(seg: &str) -> (&str, &str) {
    const KEPT: [&str; 7] = [
        "2>/dev/null",
        "2> /dev/null",
        "2>>/dev/null",
        "2>> /dev/null",
        "2>&1",
        "</dev/null",
        "< /dev/null",
    ];
    let mut end = seg.len();
    'strip: loop {
        let rest = trim_blanks(&seg[..end]).1;
        for r in KEPT {
            let Some(before) = rest.strip_suffix(r) else {
                continue;
            };
            let b = before.as_bytes();
            let escaped =
                |n: usize| b[..n].iter().rev().take_while(|&&c| c == b'\\').count() % 2 == 1;
            let redirects = if r.starts_with('<') {
                // `a</dev/null` redirects too, unless the `<` is escaped
                !escaped(b.len())
            } else {
                // the `2` starts a word: after an unescaped blank
                b.last().is_some_and(|c| matches!(c, b' ' | b'\t')) && !escaped(b.len() - 1)
            };
            if redirects {
                end = before.len();
                continue 'strip;
            }
        }
        break;
    }
    let search = trim_blanks(&seg[..end]).1;
    (search, &seg[search.len()..])
}

/// The path of a `cd PATH` alone, written without expansions.
fn cd_path(stages: &[std::ops::Range<usize>], body: &str) -> Option<String> {
    if stages.len() != 1 {
        return None;
    }
    match &shell_words(body).ok()?[..] {
        [cd, path] if cd == "cd" && !path.is_empty() && !path.starts_with(['-', '~']) => {
            Some(path.clone())
        }
        _ => None,
    }
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
    /// The regex translated from a grep pattern, for the caller to check
    /// that greeg's matcher takes it.
    pub regex: Option<String>,
    /// Another command runs before it in the line (a `cd`, for one), so its
    /// stats record could not be replayed as it ran.
    pub after_other: bool,
    /// Its output goes to a pipe, which the stats record would not replay.
    pub piped: bool,
    /// An unquoted `--include=*.rs`-style word: the caller checks that no
    /// file in the working directory is named so.
    pub unquoted_glob: bool,
    /// Whether its lines, every match, are grep's for another reader; with
    /// its only operand.
    reader: Result<Option<String>, String>,
    /// Its lines go to another reader and it names one path: the caller
    /// checks it is a directory (grep prints a lone file's lines with or
    /// without its name, by system).
    pub lone_operand: Option<String>,
    /// The `cd` paths run before it in the line, in order, or `None` when
    /// another command may have changed its directory.
    pub cds: Option<Vec<String>>,
}

/// A rewritten Bash command line.
pub struct Rewrite {
    pub command: String,
    pub parts: Vec<Part>,
    /// Every command in the line is one the hook rewrote.
    pub whole: bool,
}

/// A simple rg or recursive grep command, or each one of a `;`/`&&`/`||`
/// list, becomes greeg; every other byte of the line is kept. Exit statuses
/// agree (0 found, 1 not, 2 error), so the list's control flow is unchanged.
/// A search piped into `head` keeps it (head shows greeg's best lines); a
/// recursive grep piped into another reader gives it every match in grep's
/// format. Other pipelines and compound syntax are declined: a consumer may
/// expect the original tool's output.
pub fn rewrite_full(cmd: &str) -> Result<Rewrite, String> {
    let segments = list_segments(cmd)?;
    let mut command = String::with_capacity(cmd.len() + 64);
    let mut parts: Vec<Part> = Vec::new();
    let mut why = None;
    let mut env = false;
    // another program reads a rewritten search's output
    let mut readers = false;
    let mut cds = Some(Vec::new());
    let mut at = 0;
    for seg in segments {
        let (lead, body) = trim_blanks(&cmd[seg.stages[0].clone()]);
        if body.is_empty() && seg.stages.len() == 1 {
            continue;
        }
        let (search, redirections) = split_redirections(body);
        let rewritten = match &seg.stages[..] {
            [_] => rewrite_words(search),
            [_, then] if is_head(&cmd[then.clone()]) => rewrite_words(search).map(|mut p| {
                p.piped = true;
                p
            }),
            _ => rewrite_words(search)
                .and_then(|p| into_reader(p, redirections))
                .inspect(|_| readers = true)
                .map_err(|e| format!("pipeline: {e}")),
        }
        .and_then(|p| {
            let quiet = [
                "2>/dev/null",
                "2> /dev/null",
                "2>>/dev/null",
                "2>> /dev/null",
            ];
            if p.original[0] != "rg" && quiet.iter().any(|r| redirections.contains(r)) {
                return Err(
                    "`2>/dev/null` would hide that greeg skips ignored, hidden and binary files"
                        .into(),
                );
            }
            Ok(p)
        })
        // a `cd` before it may change the directory its glob would match in
        .and_then(|p| match why {
            Some(_) if p.unquoted_glob => {
                Err("an unquoted glob after another command, in a directory not known yet".into())
            }
            _ => Ok(p),
        });
        match rewritten {
            Ok(mut p) => {
                p.after_other = why.is_some();
                p.cds = cds.clone();
                let from = seg.stages[0].start + lead;
                command.push_str(&cmd[at..from]);
                command.push_str(&p.command);
                at = from + search.len();
                parts.push(p);
            }
            // left as it is; a search's reason says more than another's
            Err(e) => {
                cds = cds.and_then(|mut cds| {
                    cds.push(cd_path(&seg.stages, body)?);
                    Some(cds)
                });
                env |= seg
                    .stages
                    .iter()
                    .any(|r| changes_environment(trim_blanks(&cmd[r.clone()]).1));
                let search = shell_words(body).is_ok_and(|w| {
                    w.first()
                        .is_some_and(|f| matches!(f.as_str(), "rg" | "grep" | "egrep" | "fgrep"))
                });
                if search && !why.as_ref().is_some_and(|(was, _)| *was) || why.is_none() {
                    why = Some((search, e));
                }
            }
        }
        // a later command runs in the `cd`s' directory only if each of them
        // succeeded: every separator after one is `&&`
        let end = seg.stages.last().map_or(0, |r| r.end);
        let and = cmd[end..].trim_start_matches([' ', '\t']).starts_with("&&");
        if !and && cds.as_ref().is_some_and(|c| !c.is_empty()) {
            cds = None;
        }
    }
    if parts.is_empty() {
        return Err(why.map_or_else(|| "not an rg or grep command".into(), |(_, e)| e));
    }
    if env {
        return Err("another command in the line can change the environment".into());
    }
    command.push_str(&cmd[at..]);
    let whole = why.is_none() && !readers;
    Ok(Rewrite {
        command,
        parts,
        whole,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_expressions_become_the_rust_regex_that_matches_the_same_lines() {
        for (posix, extended, rust) in [
            // basic: operators are escaped, literals are not
            ("fn \\|#\\[ignore", false, "fn |#\\[ignore"),
            ("a\\(b\\)*c", false, "a(b)*c"),
            ("a+b?c|d(e){f}", false, "a\\+b\\?c\\|d\\(e\\)\\{f\\}"),
            ("ab\\+c\\?", false, "ab+c?"),
            ("x\\{2,3\\}", false, "x{2,3}"),
            ("x\\{,3\\}", false, "x{0,3}"),
            ("*star", false, "\\*star"),
            ("^*", false, "^\\*"),
            ("a^b$c", false, "a\\^b\\$c"),
            ("^a$", false, "^a$"),
            ("\\(^a\\|b$\\)", false, "(^a|b$)"),
            ("\\<word\\>", false, "\\<word\\>"),
            ("\\.\\*\\/", false, "\\.\\*\\/"),
            // extended: the operators are plain
            ("a+|b?(c){2}", true, "a+|b?(c){2}"),
            ("+x", true, "\\+x"),
            ("a{", true, "a\\{"),
            ("a{x}", true, "a\\{x\\}"),
            ("a)", true, "a\\)"),
            ("(a|)", true, "(a|)"),
            ("\\(lit\\)", true, "\\(lit\\)"),
            ("\\bx\\s+\\w", true, "\\bx\\s+\\w"),
            // bracket expressions
            ("[]a]", false, "[\\]a]"),
            ("[^]a]", false, "[^\\]a]"),
            ("[[:alpha:]_]", true, "[[:alpha:]_]"),
            ("[a\\b]", false, "[a\\\\b]"),
            ("[a[b]", false, "[a\\[b]"),
            ("[&~]", true, "[\\&\\~]"),
            ("[a-z0-9]", false, "[a-z0-9]"),
            ("é+", true, "é+"),
        ] {
            assert_eq!(
                posix_regex(posix, extended).as_deref(),
                Ok(rust),
                "{posix:?} {extended}"
            );
        }
        for (posix, extended, why) in [
            ("\\(a\\)\\1", false, "back-reference"),
            ("(a)\\1", true, "back-reference"),
            ("\\d+", true, "not grep syntax"),
            ("a\\", false, "trailing backslash"),
            ("[abc", false, "unterminated"),
            ("[[:word:]]", false, "not a character class"),
            ("[[=a=]]", false, "collating"),
            ("[a--b]", false, "`--`"),
            ("\\(a", false, "unmatched ("),
            ("a\\)", false, "unmatched"),
            ("(a", true, "unmatched ("),
            ("a\\{x\\}", false, "malformed"),
        ] {
            let got = posix_regex(posix, extended).err().unwrap_or_default();
            assert!(got.contains(why), "{posix:?}: {got:?}");
        }
    }

    #[test]
    fn recursive_greps_become_greeg_searches() {
        let g = "greeg --matching exact --max-filesize 18446744073709551615 --from-grep";
        for (cmd, want) in [
            ("grep -rn needle .", format!("{g} needle .")),
            (
                "grep -rn 'fn \\|struct ' src",
                format!("{g} 'fn |struct ' src"),
            ),
            // with no file operand grep -r searches the working directory
            ("grep -rniw Foo", format!("{g} -i -w Foo .")),
            ("grep -rl needle src lib", format!("{g} -l needle src lib")),
            ("grep -rnE 'a+b' .", format!("{g} a+b .")),
            ("egrep -r 'a|b' .", format!("{g} 'a|b' .")),
            ("grep -rF 'a.b(' .", format!("{g} -F 'a.b(' .")),
            ("fgrep -r x .", format!("{g} -F x .")),
            ("grep -rn -A 3 -B2 x .", format!("{g} -A 3 -B 2 x .")),
            (
                "grep -r --include=*.rs --exclude-dir=target x .",
                format!("{g} -g '*.rs' -g '!target/' x ."),
            ),
            (
                "grep -r --exclude='*.lock' --include '*.ts' x",
                format!("{g} -g '*.ts' -g '!*.lock' x ."),
            ),
            ("grep -rn -e -dash .", format!("{g} -- -dash .")),
            ("grep -rn def .", format!("{g} -e def .")),
            ("grep -rIH --color=never x .", format!("{g} x .")),
            ("grep -r --colour x .", format!("{g} x .")),
            ("grep -rE -E x .", format!("{g} x .")),
            (
                "grep --recursive --ignore-case --files-with-matches x .",
                format!("{g} -i -l x ."),
            ),
        ] {
            let r = rewrite_full(cmd).unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
            assert_eq!(r.command, want, "{cmd:?}");
            assert_eq!(
                shell_words(&r.command).unwrap(),
                r.parts[0].rewritten,
                "{cmd:?}"
            );
        }
        for (cmd, why) in [
            ("grep needle file.rs", "without -r"),
            ("grep -rv x .", "`-v`"),
            ("grep -rq x .", "`-q`"),
            ("grep -ro x .", "`-o`"),
            ("grep -rL x .", "`-L`"),
            ("grep -rm1 x .", "`-m`"),
            ("grep -rh x .", "file names"),
            ("grep -r --null x .", "`--null`"),
            ("grep -r -e a -e b .", "several -e"),
            ("grep -r --binary-files=text x .", "`--binary-files`"),
            ("grep -r --include='*.{ts,tsx}' x .", "no glob equivalent"),
            ("grep -r --include=*.{ts,tsx} x .", "brace"),
            ("grep -r x -", "stdin"),
            ("grep -rA x y .", "whole number"),
            ("/usr/bin/grep -r x .", "explicit executable path"),
            ("grep -rE -F x .", "conflicting matchers"),
            ("grep -rF -G x .", "conflicting matchers"),
            ("egrep -rF x .", "conflicting matchers"),
            ("fgrep -rE x .", "conflicting matchers"),
            ("grep -r --color=bogus x .", "not a color mode"),
            ("grep -rs x .", "would hide greeg's note"),
            ("grep -r --no-messages x .", "would hide greeg's note"),
        ] {
            let got = rewrite_full(cmd).err().unwrap_or_default();
            assert!(got.contains(why), "{cmd:?}: {got:?}");
        }
    }
}
