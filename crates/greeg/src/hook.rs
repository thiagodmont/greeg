//! `greeg hook claude`: install a Claude Code PreToolUse hook that rewrites
//! `rg`/`grep` Bash calls to `greeg`, plus a short skill file. `greeg hook run`
//! is the hook: it reads the tool call JSON on stdin and prints an
//! `updatedInput` when the command is a plain rg/grep invocation it can map.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::PathBuf;

const SKILL: &str = "---
name: greeg
description: Code search for agents. Use `greeg` instead of rg/grep: ranked, syntax-aware, budgeted results, plus symbol verbs (def, refs, callers, impls, outline, map, impact).
---

# greeg

`greeg PATTERN` accepts ripgrep flags (`-i -w -F -t rs -g '*.py' -A/-B/-C --json`) and
returns hits classified as def/import/call/type/member/ident/doc/comment/string with
the enclosing symbol, tests and vendored code demoted, within a token budget
(`--budget N`, default 2000). Broad queries return facets first; the footer says what
was cut and suggests the next query. Exit 1 means no hits (after the escalation
ladder: word boundary, case, split tokens, fuzzy names).

Symbol questions are cheaper than searches:

    greeg def NAME              where it is defined (signature, doc, reachability)
    greeg refs NAME             references grouped by kind
    greeg callers NAME --depth 2
    greeg impls NAME            implementations / subclasses
    greeg outline FILE          definitions of a file as a tree
    greeg map DIR               important files by import PageRank
    greeg impact NAME           WILL / MAY BREAK / REVIEW if NAME changes

Use `-e PATTERN` when the pattern is a verb name. `--budget 0` gives unlimited,
rg-ordered output. `greeg doctor` shows index state.
";

fn settings_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".claude/settings.json"))
}

fn skill_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".claude/skills/greeg/SKILL.md"))
}

fn hook_entry() -> Value {
    json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "greeg hook run"}]})
}

fn is_ours(v: &Value) -> bool {
    v.get("hooks").and_then(|h| h.as_array()).map(|a| a.iter().any(|h| h.get("command").and_then(|c| c.as_str()).map(|c| c.starts_with("greeg hook")).unwrap_or(false))).unwrap_or(false)
}

pub fn install_claude(uninstall: bool, dry_run: bool) -> Result<()> {
    let sp = settings_path()?;
    let kp = skill_path()?;
    let mut settings: Value = match std::fs::read_to_string(&sp) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse {}", sp.display()))?,
        Err(_) => json!({}),
    };
    let obj = settings.as_object_mut().context("settings.json is not an object")?;
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    let pre = hooks.as_object_mut().context("hooks is not an object")?.entry("PreToolUse").or_insert_with(|| json!([]));
    let arr = pre.as_array_mut().context("PreToolUse is not an array")?;
    let had = arr.iter().any(is_ours);
    if uninstall {
        arr.retain(|v| !is_ours(v));
    } else if !had {
        // first, so the rewrite happens before other Bash hooks see the command
        arr.insert(0, hook_entry());
    }
    let out = serde_json::to_string_pretty(&settings)?;
    if dry_run {
        println!("{}: {}", sp.display(), if uninstall { if had { "would remove the greeg hook" } else { "no greeg hook present" } } else if had { "hook already installed" } else { "would add PreToolUse hook `greeg hook run` (matcher Bash)" });
        println!("{}: {}", kp.display(), if uninstall { "would remove" } else { "would write the greeg skill" });
        return Ok(());
    }
    if let Some(p) = sp.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&sp, out)?;
    if uninstall {
        let _ = std::fs::remove_file(&kp);
        println!("removed the greeg hook from {} and {}", sp.display(), kp.display());
    } else {
        if let Some(p) = kp.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&kp, SKILL)?;
        println!("{}: {}\n{}: skill written\nrestart Claude Code (or /hooks) to pick up the hook; test with: echo '{{\"tool_name\":\"Bash\",\"tool_input\":{{\"command\":\"rg -n foo src\"}}}}' | greeg hook run", sp.display(), if had { "hook already installed" } else { "PreToolUse hook `greeg hook run` added" }, kp.display());
    }
    Ok(())
}

/// Split a shell command segment into words (quotes and backslashes honoured).
fn shell_words(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for d in chars.by_ref() {
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
            '$' | '`' | '<' | '>' | ';' | '&' | '(' | ')' | '\n' => return None,
            // an unquoted glob or `~` in a path would be expanded by the shell: leave it alone
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

fn quote(w: &str) -> String {
    if !w.is_empty() && w.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./:=,@+".contains(&b)) {
        w.to_string()
    } else {
        format!("'{}'", w.replace('\'', "'\\''"))
    }
}

/// Rewrite one pipeline segment; `None` = leave it alone.
pub fn rewrite_segment(seg: &str) -> Option<String> {
    let words = shell_words(seg.trim())?;
    if words.is_empty() {
        return None;
    }
    let prog = words[0].rsplit('/').next().unwrap_or(&words[0]);
    let (is_grep, fixed_default) = match prog {
        "rg" => (false, false),
        "grep" | "egrep" => (true, false),
        "fgrep" => (true, true),
        _ => return None,
    };
    let mut out: Vec<String> = vec!["greeg".into()];
    let mut positional: Vec<String> = Vec::new();
    let mut i = 1;
    let mut fixed = fixed_default;
    let mut after_dd = false;
    let takes_value = |f: &str| matches!(f, "-A" | "-B" | "-C" | "-g" | "-t" | "-T" | "-e" | "-m" | "-j" | "--max-columns" | "--max-filesize" | "--include" | "--exclude" | "--exclude-dir" | "--type" | "--glob" | "--regexp" | "--after-context" | "--before-context" | "--context" | "--threads");
    while i < words.len() {
        let w = words[i].clone();
        if after_dd || !w.starts_with('-') || w == "-" {
            positional.push(w);
            i += 1;
            continue;
        }
        if w == "--" {
            after_dd = true;
            i += 1;
            continue;
        }
        // unsupported semantics: do not rewrite
        if matches!(w.as_str(), "-v" | "--invert-match" | "-o" | "--only-matching" | "-z" | "-L" | "--files-without-match" | "-q" | "--quiet" | "--files" | "-r" if !is_grep) || matches!(w.as_str(), "-v" | "--invert-match" | "-o" | "--only-matching" | "-z" | "-L" | "-q" | "--quiet" | "--null-data" | "-P" | "--perl-regexp" | "-x" if is_grep && w == "-P") || w.starts_with("--replace") || w == "-r" && !is_grep {
            return None;
        }
        let (flag, inline_val) = match w.split_once('=') {
            Some((f, v)) if w.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (w.clone(), None),
        };
        // grep-only flags
        if is_grep {
            match flag.as_str() {
                "-r" | "-R" | "--recursive" | "-H" | "--with-filename" | "-h" | "--no-filename" | "-E" | "--extended-regexp" | "-G" | "--basic-regexp" | "-s" | "--no-messages" | "--color" | "--colour" | "-a" | "--text" | "-I" | "--binary-files" => {
                    if (flag == "--color" || flag == "--colour" || flag == "--binary-files") && inline_val.is_none() {
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
                "-F" | "--fixed-strings" => {
                    fixed = true;
                    i += 1;
                    continue;
                }
                "--include" | "--exclude" => {
                    let v = inline_val.clone().or_else(|| words.get(i + 1).cloned())?;
                    out.push("-g".into());
                    out.push(if flag == "--include" { v } else { format!("!{v}") });
                    i += if inline_val.is_some() { 1 } else { 2 };
                    continue;
                }
                "--exclude-dir" => {
                    let v = inline_val.clone().or_else(|| words.get(i + 1).cloned())?;
                    out.push("-g".into());
                    out.push(format!("!{}/**", v.trim_end_matches('/')));
                    i += if inline_val.is_some() { 1 } else { 2 };
                    continue;
                }
                _ => {}
            }
        }
        // combined short flags like -rn or -in: expand
        if flag.len() > 2 && !flag.starts_with("--") && flag[1..].chars().all(|c| c.is_ascii_alphabetic()) {
            let mut expanded: Vec<String> = flag[1..].chars().map(|c| format!("-{c}")).collect();
            expanded.reverse();
            let rest: Vec<String> = words[i + 1..].to_vec();
            let mut nw = words[..i].to_vec();
            for e in expanded.iter().rev() {
                nw.push(e.clone());
            }
            nw.extend(rest);
            return rewrite_words(nw, is_grep, fixed_default);
        }
        // rg flags greeg does not know: drop the cosmetic ones, refuse the rest
        match flag.as_str() {
            "--color" | "--colour" | "--heading" | "--no-heading" | "-N" | "--no-line-number" | "--line-number" | "-n" | "--column" | "--no-column" | "--pretty" | "-p" | "--sort" | "--sortr" | "--stats" | "--no-messages" | "--trim" | "--vimgrep" | "-H" | "--with-filename" | "--no-filename" => {
                if (flag == "--color" || flag == "--colour" || flag == "--sort" || flag == "--sortr") && inline_val.is_none() {
                    i += 1;
                }
                i += 1;
                continue;
            }
            "-i" | "--ignore-case" | "-S" | "--smart-case" | "-s" | "--case-sensitive" | "-w" | "--word-regexp" | "-x" | "--line-regexp" | "-U" | "--multiline" | "-l" | "--files-with-matches" | "-c" | "--count" | "--no-ignore" | "--hidden" | "--json" => {
                out.push(flag.clone());
                i += 1;
                continue;
            }
            "-F" | "--fixed-strings" => {
                fixed = true;
                i += 1;
                continue;
            }
            f if takes_value(f) => {
                let v = inline_val.clone().or_else(|| words.get(i + 1).cloned())?;
                let f = match f {
                    "--include" => "-g",
                    "-m" => return None,
                    x => x,
                };
                out.push(f.to_string());
                out.push(v);
                i += if inline_val.is_some() { 1 } else { 2 };
                continue;
            }
            _ => return None,
        }
    }
    if fixed {
        out.push("-F".into());
    }
    // grep and rg take PATTERN then PATHS; greeg is the same, but a pattern that looks like a verb needs -e
    let mut pos = positional.into_iter();
    let pattern = if out.iter().any(|w| w == "-e") { None } else { Some(pos.next()?) };
    if let Some(p) = pattern {
        if matches!(p.as_str(), "def" | "refs" | "callers" | "impls" | "outline" | "map" | "impact" | "index" | "doctor" | "man" | "hook" | "lang") || p.starts_with('-') {
            out.push("-e".into());
        }
        out.push(p);
    }
    for p in pos {
        if p == "." || p == "./" {
            continue;
        }
        out.push(p);
    }
    Some(out.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" "))
}

fn rewrite_words(words: Vec<String>, is_grep: bool, fixed: bool) -> Option<String> {
    let mut seg = words.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ");
    if is_grep && fixed && !words.iter().any(|w| w == "-F") {
        seg.push_str(" -F");
    }
    rewrite_segment(&seg)
}

/// Rewrite a whole Bash command: only the first pipeline segment, optionally
/// after a leading `cd DIR &&`; everything else is left untouched.
pub fn rewrite_command(cmd: &str) -> Option<String> {
    let cmd = cmd.trim();
    let (prefix, rest) = match cmd.find("&&") {
        Some(p) if cmd[..p].trim_start().starts_with("cd ") && !cmd[..p].contains('|') => (&cmd[..p + 2], &cmd[p + 2..]),
        _ => ("", cmd),
    };
    let (first, tail) = match rest.find(" | ") {
        Some(p) => (&rest[..p], &rest[p..]),
        None => (rest, ""),
    };
    if first.contains("||") || first.contains(';') {
        return None;
    }
    let new = rewrite_segment(first)?;
    Some(format!("{}{}{}{}", prefix, if prefix.is_empty() { "" } else { " " }, new, tail))
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
    let Some(cmd) = v.get("tool_input").and_then(|t| t.get("command")).and_then(|c| c.as_str()) else { return Ok(()) };
    if let Some(new) = rewrite_command(cmd)
        && new != cmd
    {
        let out = json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecisionReason": "greeg rewrite", "updatedInput": {"command": new}}});
        let mut w = std::io::stdout().lock();
        serde_json::to_writer(&mut w, &out)?;
        writeln!(w)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites() {
        assert_eq!(rewrite_command("rg -n foo src").as_deref(), Some("greeg foo src"));
        assert_eq!(rewrite_command("rg -in 'get queryset' --type py").as_deref(), Some("greeg -i --type py 'get queryset'"));
        assert_eq!(rewrite_command("grep -rn \"TODO\" . --include=*.rs").as_deref(), Some("greeg -g '*.rs' TODO"));
        assert_eq!(rewrite_command("grep -rnw foo src/ | head -20").as_deref(), Some("greeg -w foo src/ | head -20"));
        assert_eq!(rewrite_command("cd /tmp/x && rg foo").as_deref(), Some("cd /tmp/x && greeg foo"));
        assert_eq!(rewrite_command("rg def --type rs").as_deref(), Some("greeg --type rs -e def"));
        assert_eq!(rewrite_command("rg -o 'x' src"), None);
        assert_eq!(rewrite_command("rg -v foo"), None);
        assert_eq!(rewrite_command("rg foo $DIR"), None);
        assert_eq!(rewrite_command("git status"), None);
        assert_eq!(rewrite_command("rg --files"), None);
        assert_eq!(rewrite_command("rg -C 3 'fn main' crates").as_deref(), Some("greeg -C 3 'fn main' crates"));
        assert_eq!(rewrite_command("fgrep -r needle lib").as_deref(), Some("greeg -F needle lib"));
    }
}
