//! `greeg hook claude`: install a Claude Code PreToolUse hook that rewrites
//! `rg`/`grep` Bash calls to `greeg`, plus a short skill file. `greeg hook run`
//! is the hook: it reads the tool call JSON on stdin and prints an
//! `updatedInput` when the command is a plain rg/grep invocation it can map.
//!
//! The rewriter is conservative: it touches only the first pipeline segment
//! (optionally after a leading `cd DIR &&`), never drops positional
//! arguments, and leaves the command alone whenever a flag has semantics
//! greeg does not implement or the shell would expand something.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::PathBuf;

const SKILL: &str = "---
name: greeg
description: Code search for agents. Use `greeg` instead of rg/grep: ranked, syntax-aware, budgeted results, plus symbol verbs (def, refs, callers, impls, outline, map, impact).
---

# greeg

`greeg PATTERN [PATHS]` accepts ripgrep flags (`-i -w -F -t rs -g '*.py' -A/-B/-C --json`)
and returns hits classified as def/import/call/type/member/ident/doc/comment/string with
the enclosing symbol, tests and vendored code demoted, within a token budget
(`--budget N`, default 2000). Broad queries return facets first; the footer says what
was cut and suggests the next query. Exit 1 means no hits (after the escalation
ladder: word boundary, case, split tokens, fuzzy names).

`-l` prints only paths and `-c` prints `path:count`, one per line on stdout, so both
are pipe-safe: `greeg -l foo | xargs sed -i ...`, `greeg -c foo | sort -t: -k2 -n`.

Symbol questions are cheaper than searches:

    greeg def NAME              where it is defined (signature, doc, reachability)
    greeg refs NAME             references grouped by kind
    greeg callers NAME --depth 2
    greeg impls NAME            implementations / subclasses
    greeg outline FILE          definitions of a file as a tree
    greeg map DIR               important files by import PageRank
    greeg impact NAME           WILL / MAY BREAK / REVIEW if NAME changes

Use `-e PATTERN` when the pattern is a verb name (`greeg -e def src`). When the pattern
or a path starts with `-`, put `--` before it: `greeg -- -x src`, `greeg foo -- -weird`.
`--budget 0` gives unlimited, rg-ordered output. `greeg doctor` shows index state.

## Hook notes

The installed PreToolUse hook (`greeg hook run`) rewrites plain `rg`/`grep` Bash
calls into `greeg`. PreToolUse hooks run in parallel and the last `updatedInput`
wins, so another Bash rewriter (for example RTK) can override or be overridden by
this one; position in `settings.json` does not order them. Permission allow rules
such as `Bash(rg:*)` no longer match the rewritten command: pair them with
`Bash(greeg:*)`.
";

fn settings_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".claude/settings.json"))
}

fn skill_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".claude/skills/greeg/SKILL.md"))
}

/// The settings.json entry. Note: PreToolUse hooks run in parallel and the
/// last `updatedInput` wins, so inserting first does not order this hook
/// before other Bash rewriters; `Bash(rg:*)` allow rules should be paired
/// with `Bash(greeg:*)` because the rewritten command no longer matches them.
fn hook_entry() -> Value {
    json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "greeg hook run"}]})
}

fn is_ours(v: &Value) -> bool {
    v.get("hooks")
        .and_then(|h| h.as_array())
        .map(|a| {
            a.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(|c| c.starts_with("greeg hook"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

pub fn install_claude(uninstall: bool, dry_run: bool) -> Result<()> {
    let sp = settings_path()?;
    let kp = skill_path()?;
    let mut settings: Value = match std::fs::read_to_string(&sp) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse {}", sp.display()))?,
        Err(_) => json!({}),
    };
    let obj = settings
        .as_object_mut()
        .context("settings.json is not an object")?;
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    let pre = hooks
        .as_object_mut()
        .context("hooks is not an object")?
        .entry("PreToolUse")
        .or_insert_with(|| json!([]));
    let arr = pre.as_array_mut().context("PreToolUse is not an array")?;
    let had = arr.iter().any(is_ours);
    if uninstall {
        arr.retain(|v| !is_ours(v));
    } else if !had {
        // first for readability; hooks run in parallel, so this is not an ordering guarantee
        arr.insert(0, hook_entry());
    }
    let out = serde_json::to_string_pretty(&settings)?;
    if dry_run {
        println!(
            "{}: {}",
            sp.display(),
            if uninstall {
                if had {
                    "would remove the greeg hook"
                } else {
                    "no greeg hook present"
                }
            } else if had {
                "hook already installed"
            } else {
                "would add PreToolUse hook `greeg hook run` (matcher Bash)"
            }
        );
        println!(
            "{}: {}",
            kp.display(),
            if uninstall {
                "would remove"
            } else {
                "would write the greeg skill"
            }
        );
        return Ok(());
    }
    if let Some(p) = sp.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&sp, out)?;
    if uninstall {
        let _ = std::fs::remove_file(&kp);
        println!(
            "removed the greeg hook from {} and {}",
            sp.display(),
            kp.display()
        );
    } else {
        if let Some(p) = kp.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&kp, SKILL)?;
        println!(
            "{}: {}\n{}: skill written\nrestart Claude Code (or /hooks) to pick up the hook; test with: echo '{{\"tool_name\":\"Bash\",\"tool_input\":{{\"command\":\"rg -n foo src\"}}}}' | greeg hook run\nnote: PreToolUse hooks run in parallel (last updatedInput wins); pair Bash(rg:*) allow rules with Bash(greeg:*)",
            sp.display(),
            if had {
                "hook already installed"
            } else {
                "PreToolUse hook `greeg hook run` added"
            },
            kp.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// shell tokenizing
// ---------------------------------------------------------------------------

/// A top-level (unquoted) control operator or redirection in a command line.
struct OpAt {
    start: usize,
    end: usize,
    text: &'static str,
    redirect: bool,
}

/// Find top-level `|`, `||`, `&&`, `;`, newline, `&` and redirections
/// (`>`, `>>`, `<`, `&>`; `2>` is seen as `>`) outside quotes. `None` on an
/// unterminated quote.
fn scan_ops(cmd: &str) -> Option<Vec<OpAt>> {
    let b = cmd.as_bytes();
    let mut ops = Vec::new();
    let mut i = 0;
    let next = |i: usize| b.get(i + 1).copied();
    while i < b.len() {
        let mut push = |text: &'static str, len: usize, redirect: bool| {
            ops.push(OpAt {
                start: i,
                end: i + len,
                text,
                redirect,
            })
        };
        match b[i] {
            b'\'' => {
                i += 1;
                while b.get(i)? != &b'\'' {
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                loop {
                    match b.get(i)? {
                        b'"' => break,
                        b'\\' => i += 2,
                        _ => i += 1,
                    }
                }
            }
            b'\\' => i += 1,
            b'|' if next(i) == Some(b'|') => {
                push("||", 2, false);
                i += 1;
            }
            b'|' => push("|", 1, false),
            b'&' if next(i) == Some(b'&') => {
                push("&&", 2, false);
                i += 1;
            }
            b'&' if next(i) == Some(b'>') => {
                push("&>", 2, true);
                i += 1;
            }
            b'&' => push("&", 1, false),
            b';' | b'\n' => push(";", 1, false),
            b'>' if next(i) == Some(b'>') => {
                push(">>", 2, true);
                i += 1;
            }
            b'>' => push(">", 1, true),
            b'<' => push("<", 1, true),
            _ => {}
        }
        i += 1;
    }
    Some(ops)
}

/// Split one pipeline segment into words (quotes and backslashes honoured).
/// `None` when the shell would expand or interpret something we do not model.
fn shell_words(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    let d = chars.next()?;
                    if d == '\'' {
                        break;
                    }
                    cur.push(d);
                }
            }
            '"' => {
                in_word = true;
                loop {
                    let d = chars.next()?;
                    match d {
                        '"' => break,
                        '\\' => {
                            let e = chars.next()?;
                            if !matches!(e, '"' | '\\' | '$' | '`') {
                                cur.push('\\');
                            }
                            cur.push(e);
                        }
                        '$' | '`' => return None, // expansions: leave the command alone
                        _ => cur.push(d),
                    }
                }
            }
            '\\' => {
                in_word = true;
                cur.push(chars.next()?);
            }
            ' ' | '\t' => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            // redirections, expansions and control operators: not a plain invocation
            '$' | '`' | '<' | '>' | ';' | '&' | '|' | '(' | ')' | '{' | '}' | '\n' => return None,
            // an unquoted glob or `~` would be expanded by the shell: leave it alone
            '*' | '?' | '[' if !cur.starts_with('-') => return None,
            '~' if !in_word => return None,
            _ => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    Some(out)
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Prog {
    Rg,
    Grep,
}

/// What to do with a flag. `Value` flags take the next word (or the attached
/// remainder of a short group); `DropValue` flags are dropped with their value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flag {
    /// pass through unchanged
    Keep,
    /// cosmetic: drop
    Drop,
    /// cosmetic with a value: drop both (`optional` = value only via `=`)
    DropValue {
        optional: bool,
    },
    /// takes a value; emitted as `name value`
    Value,
    Fixed,
    Unrestricted,
    Ere,
    Bre,
    Recursive,
    /// semantics greeg does not implement: do not rewrite
    Unsupported,
}

fn short_flag(c: char, prog: Prog) -> Flag {
    use Flag::*;
    match (c, prog) {
        ('i' | 'w' | 'x' | 'l' | 'c', _) => Keep,
        ('S' | 's' | 'U', Prog::Rg) => Keep,
        ('n' | 'N' | 'H' | 'h' | 'p', Prog::Rg) => Drop,
        ('n' | 'H' | 'h' | 's' | 'I', Prog::Grep) => Drop,
        ('F', _) => Fixed,
        ('u', Prog::Rg) => Unrestricted,
        ('E', Prog::Grep) => Ere,
        ('G', Prog::Grep) => Bre,
        ('r' | 'R', Prog::Grep) => Recursive,
        ('A' | 'B' | 'C' | 'e', _) => Value,
        ('g' | 't' | 'T' | 'j' | 'M', Prog::Rg) => Value,
        _ => Unsupported,
    }
}

fn long_flag(name: &str, prog: Prog) -> Flag {
    use Flag::*;
    match (name, prog) {
        (
            "--ignore-case"
            | "--word-regexp"
            | "--line-regexp"
            | "--files-with-matches"
            | "--count",
            _,
        ) => Keep,
        (
            "--smart-case" | "--case-sensitive" | "--multiline" | "--no-ignore" | "--hidden"
            | "--json" | "--stats",
            Prog::Rg,
        ) => Keep,
        ("--fixed-strings", _) => Fixed,
        ("--unrestricted", Prog::Rg) => Unrestricted,
        ("--extended-regexp", Prog::Grep) => Ere,
        ("--basic-regexp", Prog::Grep) => Bre,
        ("--recursive" | "--dereference-recursive", Prog::Grep) => Recursive,
        (
            "--line-number" | "--with-filename" | "--no-filename" | "--no-messages"
            | "--line-buffered",
            _,
        ) => Drop,
        (
            "--no-line-number" | "--heading" | "--no-heading" | "--column" | "--no-column"
            | "--pretty" | "--trim" | "--block-buffered" | "--vimgrep" | "--no-config" | "--mmap"
            | "--no-mmap",
            Prog::Rg,
        ) => Drop,
        ("--initial-tab", Prog::Grep) => Drop,
        ("--color" | "--colour", Prog::Rg) => DropValue { optional: false },
        ("--colors" | "--sort" | "--sortr", Prog::Rg) => DropValue { optional: false },
        ("--color" | "--colour", Prog::Grep) => DropValue { optional: true },
        ("--binary-files", Prog::Grep) => DropValue { optional: false },
        ("--after-context" | "--before-context" | "--context" | "--regexp", _) => Value,
        (
            "--glob" | "--type" | "--type-not" | "--threads" | "--max-columns" | "--max-filesize",
            Prog::Rg,
        ) => Value,
        ("--include" | "--exclude" | "--exclude-dir", Prog::Grep) => Value,
        _ => Unsupported,
    }
}

// ---------------------------------------------------------------------------
// rewriting
// ---------------------------------------------------------------------------

/// Parsed rg/grep invocation, ready to be re-emitted as `greeg`.
#[derive(Default)]
struct Parsed {
    flags: Vec<String>,
    /// patterns given with `-e`/`--regexp`
    patterns: Vec<String>,
    /// (word, seen after `--`)
    positional: Vec<(String, bool)>,
    fixed: bool,
    ere: bool,
    recursive: bool,
    unrestricted: u8,
}

impl Parsed {
    /// Apply one flag; `name` is the canonical flag as it should be emitted
    /// (`-t`, `--type`, ...), `value` its value when `kind == Value`.
    fn apply(&mut self, kind: Flag, name: &str, value: Option<String>, prog: Prog) -> Option<()> {
        match kind {
            Flag::Keep => self.flags.push(name.to_string()),
            Flag::Drop | Flag::DropValue { .. } => {}
            Flag::Fixed => self.fixed = true,
            Flag::Unrestricted => self.unrestricted += 1,
            Flag::Ere => self.ere = true,
            Flag::Bre => self.ere = false,
            Flag::Recursive => self.recursive = true,
            Flag::Unsupported => return None,
            Flag::Value => {
                let v = value?;
                match name {
                    "-e" | "--regexp" => self.patterns.push(v),
                    "-A" | "-B" | "-C" | "-j" | "--after-context" | "--before-context"
                    | "--context" | "--threads" | "--max-columns" | "--max-filesize" => {
                        v.parse::<u64>().ok()?;
                        self.flags.push(name.to_string());
                        self.flags.push(v);
                    }
                    "-M" => {
                        v.parse::<u64>().ok()?;
                        self.flags.push("--max-columns".into());
                        self.flags.push(v);
                    }
                    "--include" => {
                        self.flags.push("-g".into());
                        self.flags.push(v);
                    }
                    "--exclude" => {
                        self.flags.push("-g".into());
                        self.flags.push(format!("!{v}"));
                    }
                    "--exclude-dir" => {
                        self.flags.push("-g".into());
                        self.flags.push(format!("!{}/**", v.trim_end_matches('/')));
                    }
                    _ => {
                        debug_assert!(prog == Prog::Rg);
                        self.flags.push(name.to_string());
                        self.flags.push(v);
                    }
                }
            }
        }
        Some(())
    }
}

const VERBS: &[&str] = &[
    "def", "refs", "callers", "impls", "outline", "map", "impact", "index", "doctor", "man",
    "hook", "lang", "stats",
];

fn parse(words: &[String], prog: Prog) -> Option<Parsed> {
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
            let kind = long_flag(&name, prog);
            let value = match kind {
                Flag::Value | Flag::DropValue { optional: false } => Some(match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        words.get(i - 1)?.clone()
                    }
                }),
                _ => inline,
            };
            p.apply(kind, &name, value, prog)?;
            continue;
        }
        // short flag group: `-in`, `-tjs`, `-A3`; stops at the first flag that takes a value
        let body = &w[1..];
        if prog == Prog::Grep && body.bytes().all(|b| b.is_ascii_digit()) {
            p.apply(Flag::Value, "-C", Some(body.to_string()), prog)?;
            continue;
        }
        for (k, c) in body.char_indices() {
            let c = if prog == Prog::Grep && c == 'y' {
                'i'
            } else {
                c
            };
            let kind = short_flag(c, prog);
            let name = format!("-{c}");
            if kind == Flag::Value {
                let rest = &body[k + c.len_utf8()..];
                let value = if rest.is_empty() {
                    i += 1;
                    words.get(i - 1)?.clone()
                } else {
                    rest.to_string()
                };
                p.apply(kind, &name, Some(value), prog)?;
                break;
            }
            p.apply(kind, &name, None, prog)?;
        }
    }
    Some(p)
}

/// Translate a GNU grep BRE into an ERE by swapping the escaped/unescaped
/// meaning of `| ( ) { } + ?`. Bracket expressions are copied verbatim.
/// `None` when an exact translation is not certain.
fn bre_to_ere(pat: &str) -> Option<String> {
    const META: &[char] = &['|', '(', ')', '{', '}', '+', '?'];
    if !pat.contains(META) {
        return Some(pat.to_string());
    }
    let cs: Vec<char> = pat.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    // `*` is literal at the start of a BRE or after `\(` / `\|`: no exact ERE form
    let mut at_start = true;
    while i < cs.len() {
        let c = cs[i];
        match c {
            '\\' => {
                let d = *cs.get(i + 1)?;
                if !META.contains(&d) {
                    out.push('\\');
                }
                out.push(d);
                at_start = matches!(d, '(' | '|');
                i += 2;
            }
            '*' if at_start => return None,
            '[' => {
                let start = i;
                i += 1;
                if cs.get(i) == Some(&'^') {
                    i += 1;
                }
                if cs.get(i) == Some(&']') {
                    i += 1;
                }
                loop {
                    match *cs.get(i)? {
                        ']' => break,
                        '\\' => return None,
                        '[' if cs.get(i + 1) == Some(&':') => {
                            i += 2;
                            while *cs.get(i)? != ']' {
                                i += 1;
                            }
                        }
                        '[' => return None,
                        _ => {}
                    }
                    i += 1;
                }
                out.extend(&cs[start..=i]);
                at_start = false;
                i += 1;
            }
            _ => {
                if META.contains(&c) {
                    out.push('\\');
                }
                out.push(c);
                at_start = c == '^' && i == 0;
                i += 1;
            }
        }
    }
    Some(out)
}

/// Rewrite one pipeline segment to argv: `(original words, greeg words)`;
/// `None` = leave it alone.
fn rewrite_words(seg: &str) -> Option<(Vec<String>, Vec<String>)> {
    let words = shell_words(seg.trim())?;
    let first = words.first()?;
    // env assignments and wrappers (`FOO=1 rg`, `timeout 5 rg`, `xargs rg`, `sudo rg`, `nice rg`)
    if first.contains('=') {
        return None;
    }
    let prog_name = first.rsplit('/').next().unwrap_or(first);
    let (prog, mut p) = match prog_name {
        "rg" => (Prog::Rg, parse(&words, Prog::Rg)?),
        "grep" => (Prog::Grep, parse(&words, Prog::Grep)?),
        "egrep" => {
            let mut p = parse(&words, Prog::Grep)?;
            p.ere = true;
            (Prog::Grep, p)
        }
        "fgrep" => {
            let mut p = parse(&words, Prog::Grep)?;
            p.fixed = true;
            (Prog::Grep, p)
        }
        _ => return None,
    };
    if p.patterns.len() > 1 {
        return None; // greeg accepts a single -e today
    }
    let (pattern, pattern_dd) = match p.patterns.pop() {
        Some(e) => (e, false),
        None => {
            if p.positional.is_empty() {
                return None;
            }
            p.positional.remove(0)
        }
    };
    let paths = std::mem::take(&mut p.positional);
    // stdin readers become tree searches: leave them alone
    if paths.iter().any(|(w, _)| w == "-")
        || (prog == Prog::Grep && !p.recursive && paths.is_empty())
    {
        return None;
    }
    let pattern = if prog == Prog::Grep && !p.ere && !p.fixed {
        bre_to_ere(&pattern)?
    } else {
        pattern
    };

    let mut out: Vec<String> = vec!["greeg".into()];
    out.extend(p.flags.clone());
    match p.unrestricted {
        0 => {}
        1 => out.push("--no-ignore".into()),
        2 => out.extend(["--no-ignore".to_string(), "--hidden".to_string()]),
        _ => return None, // -uuu also searches binaries
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
    Some((words, out))
}

/// A rewritten Bash command and what changed in it (for the stats records).
pub struct Rewrite {
    pub command: String,
    /// `DIR` of a leading `cd DIR &&`, which is where greeg will run.
    pub cd: Option<String>,
    pub original: Vec<String>,
    pub rewritten: Vec<String>,
}

/// Rewrite a whole Bash command: only the first pipeline segment, optionally
/// after a leading `cd DIR &&`; everything after the next operator is kept
/// verbatim. A redirection on the rg segment means no rewrite.
#[cfg(test)]
pub fn rewrite_command(cmd: &str) -> Option<String> {
    rewrite_full(cmd).map(|r| r.command)
}

pub fn rewrite_full(cmd: &str) -> Option<Rewrite> {
    let cmd = cmd.trim();
    let ops = scan_ops(cmd)?;
    let mut prefix = String::new();
    let mut cd = None;
    let mut seg_start = 0;
    let mut idx = 0;
    if let Some(op) = ops.first()
        && op.text == "&&"
    {
        let head = cmd[..op.start].trim();
        if head.starts_with("cd ")
            && let Some(w) = shell_words(head).filter(|w| w.len() == 2)
        {
            prefix = format!("{head} && ");
            cd = Some(w[1].clone());
            seg_start = op.end;
            idx = 1;
        }
    }
    let (seg, tail) = match ops.get(idx) {
        Some(op) if op.redirect => return None,
        Some(op) => (
            &cmd[seg_start..op.start],
            Some((op.text, cmd[op.end..].trim_start())),
        ),
        None => (&cmd[seg_start..], None),
    };
    let (original, rewritten) = rewrite_words(seg)?;
    let new = rewritten
        .iter()
        .map(|w| quote(w))
        .collect::<Vec<_>>()
        .join(" ");
    let command = match tail {
        Some((op, "")) => format!("{prefix}{new} {op}"),
        Some((op, rest)) => format!("{prefix}{new} {op} {rest}"),
        None => format!("{prefix}{new}"),
    };
    Some(Rewrite {
        command,
        cd,
        original,
        rewritten,
    })
}

pub fn run() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let v: Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    if v.get("tool_name").and_then(|t| t.as_str()) != Some("Bash") {
        return Ok(());
    }
    let Some(cmd) = v
        .get("tool_input")
        .and_then(|t| t.get("command"))
        .and_then(|c| c.as_str())
    else {
        return Ok(());
    };
    if let Some(rw) = rewrite_full(cmd)
        && rw.command != cmd
    {
        let out = json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecisionReason": "greeg rewrite", "updatedInput": {"command": rw.command}}});
        let mut w = std::io::stdout().lock();
        serde_json::to_writer(&mut w, &out)?;
        writeln!(w)?;
        // opt-in stats: where greeg will run is the hook's cwd, or the `cd DIR` in front
        let base = v
            .get("cwd")
            .and_then(|c| c.as_str())
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let cwd = match &rw.cd {
            Some(d) => base.join(d),
            None => base,
        };
        let session = v.get("session_id").and_then(|s| s.as_str());
        crate::stats::record_hook(&cwd, session, &rw.original, &rw.rewritten);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rw(cmd: &str) -> Option<String> {
        rewrite_command(cmd)
    }

    #[track_caller]
    fn same(cmd: &str, want: &str) {
        assert_eq!(rw(cmd).as_deref(), Some(want), "input: {cmd}");
    }

    #[track_caller]
    fn untouched(cmd: &str) {
        assert_eq!(rw(cmd), None, "input: {cmd}");
    }

    #[test]
    fn basics() {
        same("rg -n foo src", "greeg foo src");
        same(
            "rg -in 'get queryset' --type py",
            "greeg -i --type py 'get queryset'",
        );
        same(
            "grep -rn \"TODO\" . --include=*.rs",
            "greeg -g '*.rs' TODO .",
        );
        same(
            "grep -rnw foo src/ | head -20",
            "greeg -w foo src/ | head -20",
        );
        same("cd /tmp/x && rg foo", "cd /tmp/x && greeg foo");
        same("rg def --type rs", "greeg --type rs -e def");
        same("rg -C 3 'fn main' crates", "greeg -C 3 'fn main' crates");
        same("fgrep -r needle lib", "greeg -F needle lib");
        same("/usr/bin/rg foo", "greeg foo");
        untouched("git status");
    }

    #[test]
    fn positionals_are_never_dropped() {
        same("rg foo . ../other", "greeg foo . ../other");
        same("rg foo -- -weird", "greeg foo -- -weird");
        same("rg foo 'my dir/sub' other", "greeg foo 'my dir/sub' other");
        same("rg foo \"a b\"", "greeg foo 'a b'");
        same("rg -e foo bar src", "greeg foo bar src");
    }

    #[test]
    fn leading_dash_patterns_use_double_dash() {
        same("rg -- -x src", "greeg -- -x src");
        same("rg -F -- --flag", "greeg -F -- --flag");
        same("rg -e -x src", "greeg -- -x src");
        same("rg -- def src", "greeg -- def src");
    }

    #[test]
    fn combined_short_flags() {
        same("rg -tjs foo", "greeg -t js foo");
        same("rg -tpy foo", "greeg -t py foo");
        same("rg -gX foo", "greeg -g X foo");
        same("rg -eX src", "greeg X src");
        same("rg -A3 foo", "greeg -A 3 foo");
        same("rg -B2 foo", "greeg -B 2 foo");
        same("rg -C1 foo", "greeg -C 1 foo");
        same("rg -iw foo", "greeg -i -w foo");
        same("rg -j4 foo", "greeg -j 4 foo");
        same("rg -itrs foo", "greeg -i -t rs foo");
        same("rg -inA 2 foo src", "greeg -i -A 2 foo src");
        same("rg -M 80 foo", "greeg --max-columns 80 foo");
        untouched("rg -Cx foo");
        untouched("rg -iv foo");
    }

    #[test]
    fn multiple_e_patterns_untouched() {
        untouched("rg -e a -e b");
        untouched("grep -r -e a -e b src");
    }

    #[test]
    fn operators_without_spaces() {
        same("rg foo|wc -l", "greeg foo | wc -l");
        same("rg foo|| echo none", "greeg foo || echo none");
        same("rg foo&&echo ok", "greeg foo && echo ok");
        same("rg foo; echo done", "greeg foo ; echo done");
        same("rg foo\necho done", "greeg foo ; echo done");
        same("rg 'a|b' src | head", "greeg 'a|b' src | head");
        same("rg foo | sort | uniq -c", "greeg foo | sort | uniq -c");
        untouched("rg foo|wc -l 'unterminated");
    }

    #[test]
    fn pipe_safe_list_and_count() {
        same(
            "rg -l foo | xargs sed -i 's/a/b/'",
            "greeg -l foo | xargs sed -i 's/a/b/'",
        );
        same(
            "rg -c foo src | sort -t: -k2 -n",
            "greeg -c foo src | sort -t: -k2 -n",
        );
        same("rg -l foo|xargs wc -l", "greeg -l foo | xargs wc -l");
        untouched("vim $(rg -l foo)");
        untouched("vim `rg -l foo`");
    }

    #[test]
    fn grep_bre_patterns() {
        same("grep -r 'a\\|b' src", "greeg 'a|b' src");
        same("grep -rn 'foo\\(bar\\)\\?' src", "greeg 'foo(bar)?' src");
        same("grep -r 'x\\{2,3\\}' src", "greeg 'x{2,3}' src");
        same("grep -r 'a+b' src", "greeg 'a\\+b' src");
        same("grep -r 'f(x)' src", "greeg 'f\\(x\\)' src");
        same("grep -r 'a[(]b' src", "greeg 'a[(]b' src");
        same("grep -rE 'a|b' src", "greeg 'a|b' src");
        same("egrep -r 'a|b' src", "greeg 'a|b' src");
        same("grep -rF 'a|b' src", "greeg -F 'a|b' src");
        same("grep -r 'plain\\.txt' src", "greeg 'plain\\.txt' src");
        untouched("grep -rP 'a(?=b)' src");
        untouched("grep -r '*a\\|b' src");
        untouched("grep -r 'a[\\]]\\|b' src");
        untouched("grep -r 'a\\|b\\' src");
    }

    #[test]
    fn grep_stdin_and_flags() {
        untouched("grep foo");
        untouched("grep -i foo");
        untouched("cat x | grep foo");
        same("grep -r foo", "greeg foo");
        same("grep foo file.rs", "greeg foo file.rs");
        same("grep -rn --color=always foo src", "greeg foo src");
        same(
            "grep -r --exclude-dir=node_modules foo .",
            "greeg -g '!node_modules/**' foo .",
        );
        same(
            "grep -r --exclude '*.min.js' foo .",
            "greeg -g '!*.min.js' foo .",
        );
        same("grep -r -3 foo src", "greeg -C 3 foo src");
        same("grep -ry foo src", "greeg -i foo src");
        untouched("grep -rv foo src");
        untouched("grep -ro foo src");
        untouched("grep -rl foo -");
    }

    #[test]
    fn cosmetic_flags_dropped() {
        same("rg -nH foo", "greeg foo");
        same("rg -N foo", "greeg foo");
        same("rg -h foo", "greeg foo");
        same("rg --no-heading --heading foo", "greeg foo");
        same("rg --color never foo", "greeg foo");
        same("rg --color=never --colors 'path:fg:red' foo", "greeg foo");
        same("rg -p --column --no-column foo", "greeg foo");
        same("rg --sort path --sortr=modified foo", "greeg foo");
        same("rg --no-messages --trim --line-buffered foo", "greeg foo");
        same("rg -u foo", "greeg --no-ignore foo");
        same("rg -uu foo", "greeg --no-ignore --hidden foo");
        same("rg --unrestricted -i foo", "greeg -i --no-ignore foo");
        untouched("rg -uuu foo");
        untouched("rg -u -u -u foo");
    }

    #[test]
    fn unsupported_semantics_untouched() {
        for c in [
            "rg -v foo",
            "rg -o 'x' src",
            "rg --files",
            "rg -m 3 foo",
            "rg --max-count=3 foo",
            "rg --count-matches foo",
            "rg -q foo",
            "rg -z foo",
            "rg --null foo",
            "rg -L foo",
            "rg --type-add 'web:*.html' foo",
            "rg --type-list",
            "rg --replace bar foo",
            "rg -r bar foo",
            "rg --passthru foo",
            "rg --multiline-dotall foo",
            "rg --pre cat foo",
            "rg --search-zip foo",
            "rg -a foo",
            "rg --iglob '*.rs' foo",
            "rg -E utf-16 foo",
            "rg -f patterns.txt src",
        ] {
            untouched(c);
        }
        same("rg -x foo", "greeg -x foo");
        same("rg -U 'a\\nb' src", "greeg -U 'a\\nb' src");
        same("rg --json foo", "greeg --json foo");
    }

    #[test]
    fn prefixes_redirections_globs_expansions_untouched() {
        for c in [
            "FOO=1 rg foo",
            "RIPGREP_CONFIG_PATH= rg foo",
            "timeout 5 rg foo",
            "xargs rg foo",
            "sudo rg foo",
            "nice rg foo",
            "rg foo 2>/dev/null",
            "rg foo > out.txt",
            "rg foo >> out.txt",
            "rg foo < in.txt",
            "rg foo &> out.txt",
            "rg foo src/*.rs",
            "rg foo src/?.rs",
            "rg foo '[a-z]' src/[ab]",
            "rg foo $DIR",
            "rg foo \"$DIR\"",
            "rg foo `pwd`",
            "rg foo ~/src",
            "(rg foo)",
            "rg foo {a,b}",
            "cd $D && rg foo",
        ] {
            untouched(c);
        }
        same("rg 'a*b' src", "greeg 'a*b' src");
        same("rg -g '*.rs' foo", "greeg -g '*.rs' foo");
        untouched("rg foo 2>/dev/null | head");
    }

    #[test]
    fn flags_that_run_commands_are_never_rewritten() {
        // a replayed record must be a plain read-only search
        untouched("rg --pre ./script foo src");
        untouched("rg --pre=./script foo src");
        untouched("rg --pre-glob '*.pdf' --pre cat foo");
    }

    #[test]
    fn stats_is_a_verb_name() {
        same("rg stats src", "greeg -e stats src");
    }

    #[test]
    fn rewrite_full_reports_argv_and_cd() {
        let r = rewrite_full("cd crates && rg -n 'fn main' src | head").unwrap();
        assert_eq!(r.command, "cd crates && greeg 'fn main' src | head");
        assert_eq!(r.cd.as_deref(), Some("crates"));
        assert_eq!(r.original, ["rg", "-n", "fn main", "src"]);
        assert_eq!(r.rewritten, ["greeg", "fn main", "src"]);
        assert!(rewrite_full("rg foo").unwrap().cd.is_none());
    }

    #[test]
    fn bre_to_ere_exact() {
        assert_eq!(bre_to_ere("abc").as_deref(), Some("abc"));
        assert_eq!(bre_to_ere("a\\|b").as_deref(), Some("a|b"));
        assert_eq!(bre_to_ere("a|b").as_deref(), Some("a\\|b"));
        assert_eq!(bre_to_ere("\\(a\\)\\+").as_deref(), Some("(a)+"));
        assert_eq!(
            bre_to_ere("[[:alpha:]]+").as_deref(),
            Some("[[:alpha:]]\\+")
        );
        assert_eq!(bre_to_ere("[]a]+").as_deref(), Some("[]a]\\+"));
        assert_eq!(bre_to_ere("^*a\\|b"), None);
        assert_eq!(bre_to_ere("[a\\|b"), None);
        assert_eq!(bre_to_ere("a\\|b\\"), None);
    }
}
