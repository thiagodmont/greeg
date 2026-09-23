//! `greeg hook claude` / `greeg hook codex`: install a PreToolUse hook that
//! rewrites simple `rg` Bash calls to `greeg`, plus a short skill file.
//! `greeg hook run [--agent claude|codex]` is the hook: it reads the tool call
//! JSON on stdin and prints an `updatedInput` when the command is a plain
//! rg invocation it can map. Both agents send the same input shape
//! (`tool_name`, `tool_input.command`, `cwd`, `session_id`); the reply differs
//! only in that Codex applies a rewrite solely when it comes with
//! `permissionDecision: allow`, which Claude Code would read as an auto-allow.
//!
//! Only simple `rg` commands qualify. Grep dialects, compound commands,
//! explicit executable paths and shell expansion stay with the original tool.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table, TableLike};

/// The agent a hook is installed for. Shapes the hook's reply and the
/// install locations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    Claude,
    Codex,
}

const SKILL: &str = "---
name: greeg
description: Code search for agents. Use `greeg` instead of rg/grep: ranked, syntax-aware, budgeted results, plus symbol verbs (def, refs, callers, impls, outline, map, impact).
---

# greeg

`greeg PATTERN [PATHS]` accepts ripgrep flags (`-i -w -F -t rs -g '*.py' -A/-B/-C --json`)
and returns hits classified as def/import/call/type/member/ident/doc/comment/string with
the enclosing symbol, tests and vendored code demoted, within a token budget
(`--budget N`, default 2000). Broad queries return facets first; the footer says what
was cut and suggests the next query. Exit 1 means no exact hits, even if the
discovery ladder found word-boundary, case, split-token or fuzzy-name suggestions.

`-l` prints only paths and `-c` prints `path:count`, one per line on stdout, so both
use the machine output modes. Filenames are newline-delimited, not NUL-delimited.
File searches with `-l`, `-c`, `--mode files|count`, `--budget 0` or `--json`
use exact matching by default. Ranked text uses discovery. `--matching exact`
(also `--no-ladder`) disables discovery; `--matching discover` opts into it.
Relaxed file/count/unlimited results put their rung in the stderr footer and
still exit 1. Stdin always uses exact matching.

Symbol questions are cheaper than searches:

    greeg def NAME              where it is defined (signature, doc, reachability)
    greeg refs NAME             references grouped by kind
    greeg callers NAME --depth 2
    greeg impls NAME            implementations / subclasses
    greeg outline FILE          definitions of a file as a tree
    greeg show FILE:LINE        the definition enclosing a line, whole and dedented
    greeg map DIR               important files by import PageRank
    greeg impact NAME           WILL / MAY BREAK / REVIEW if NAME changes

To read a definition's body, `greeg show FILE:LINE` prints the whole definition around a
hit and `greeg def NAME --mode block` prints every definition of NAME with its body, both
dedented and bounded by the definition's span (`--mode block` on a search does the same
for its definition hits). Prefer them to guessing a `sed -n 'A,Bp'` range; `-A/-B/-C N`
give fixed context around hits. Answers are rows only: `N hits · M files` means complete.

Use `-e PATTERN` when the pattern is a verb name (`greeg -e def src`). When the pattern
or a path starts with `-`, put `--` before it: `greeg -- -x src`, `greeg foo -- -weird`.
`--budget 0` gives unlimited, rg-ordered output. `greeg doctor` shows index state.

";

const CLAUDE_HOOK_NOTES: &str = "## Hook notes

The installed PreToolUse hook (`greeg hook run`) rewrites simple `rg` Bash
calls into `greeg --matching exact`. Grep, JSON, pipes, compound commands,
comments, expansions and explicit executable paths stay unchanged. PreToolUse hooks run in parallel and the last `updatedInput`
wins, so another Bash rewriter (for example RTK) can override or be overridden by
this one; position in `settings.json` does not order them. Permission allow rules
such as `Bash(rg:*)` no longer match the rewritten command: pair them with
`Bash(greeg:*)`.
";

const CODEX_HOOK_NOTES: &str = "## Hook notes

The installed PreToolUse hook (`greeg hook run --agent codex`) rewrites plain
`rg` shell calls into `greeg --matching exact`. Grep, JSON, pipes, compound
commands, comments, expansions and explicit executable paths stay unchanged. PreToolUse hooks run in parallel and the
rewrite from the hook that finishes last wins, so another shell rewriter (for
example RTK) can override or be overridden by this one; position in `config.toml`
does not order them.
";

fn skill_text(agent: Agent) -> String {
    let notes = match agent {
        Agent::Claude => CLAUDE_HOOK_NOTES,
        Agent::Codex => CODEX_HOOK_NOTES,
    };
    format!("{SKILL}{notes}")
}

fn settings_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".claude/settings.json"))
}

fn skill_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".claude/skills/greeg/SKILL.md"))
}

/// Codex's config directory: `$CODEX_HOME`, else `~/.codex`.
fn codex_home() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("CODEX_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".codex"))
}

/// The settings.json entry. Note: PreToolUse hooks run in parallel and the
/// last `updatedInput` wins, so inserting first does not order this hook
/// before other Bash rewriters; `Bash(rg:*)` allow rules should be paired
/// with `Bash(greeg:*)` because the rewritten command no longer matches them.
fn hook_entry() -> Value {
    json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "greeg hook run"}]})
}

fn owned_command(command: &str, agent: Agent) -> bool {
    match agent {
        Agent::Claude => matches!(command, "greeg hook run" | "greeg hook run --agent claude"),
        Agent::Codex => command == CODEX_HOOK_COMMAND,
    }
}

fn json_handler_owned(handler: &Value, agent: Agent) -> bool {
    handler.get("type").and_then(Value::as_str) == Some("command")
        && handler
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| owned_command(command, agent))
}

fn is_ours(entry: &Value, agent: Agent) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|handlers| handlers.iter().any(|h| json_handler_owned(h, agent)))
}

fn edit_claude_config(text: Option<&str>, uninstall: bool) -> Result<(String, bool)> {
    let mut settings: Value = match text {
        Some(text) => serde_json::from_str(text).context("parse settings.json")?,
        None => json!({}),
    };
    let original = text.unwrap_or("");
    let obj = settings
        .as_object_mut()
        .context("settings.json is not an object")?;
    if uninstall && obj.get("hooks").is_none() {
        return Ok((original.to_owned(), false));
    }
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().context("hooks is not an object")?;
    if uninstall && hooks.get("PreToolUse").is_none() {
        return Ok((original.to_owned(), false));
    }
    let pre = hooks.entry("PreToolUse").or_insert_with(|| json!([]));
    let arr = pre.as_array_mut().context("PreToolUse is not an array")?;
    let had = arr.iter().any(|entry| {
        is_ours(entry, Agent::Claude)
            && (uninstall || entry.get("matcher").and_then(Value::as_str) == Some("Bash"))
    });
    if uninstall {
        if !had {
            return Ok((original.to_owned(), false));
        }
        arr.retain_mut(|entry| {
            if !is_ours(entry, Agent::Claude) {
                return true;
            }
            let handlers = entry
                .get_mut("hooks")
                .and_then(Value::as_array_mut)
                .unwrap();
            handlers.retain(|h| !json_handler_owned(h, Agent::Claude));
            !(handlers.is_empty()
                && entry.as_object().is_some_and(|e| {
                    e.len() == 1
                        || (e.len() == 2
                            && e.get("matcher").and_then(Value::as_str) == Some("Bash"))
                }))
        });
    } else if had {
        return Ok((original.to_owned(), true));
    } else {
        arr.push(hook_entry());
    }
    Ok((serde_json::to_string_pretty(&settings)?, had))
}

/// A matcher that lets a PreToolUse hook see Bash calls: empty, catch-all,
/// or naming `Bash` (Codex matchers are regexes, `^Bash$` included).
fn sees_bash(matcher: &str) -> bool {
    matcher.is_empty() || matcher == "*" || matcher == ".*" || matcher.contains("Bash")
}

/// Commands of the other PreToolUse hooks that see Bash calls, from a
/// `hooks.PreToolUse` JSON array (Claude Code `settings.json`, Codex `hooks.json`).
fn json_bash_hooks(v: &Value, out: &mut Vec<String>) {
    let entries = v
        .get("hooks")
        .and_then(|h| h.get("PreToolUse"))
        .and_then(|a| a.as_array());
    for entry in entries.into_iter().flatten() {
        let matcher = entry.get("matcher").and_then(|m| m.as_str()).unwrap_or("");
        if !sees_bash(matcher) {
            continue;
        }
        let hooks = entry.get("hooks").and_then(|h| h.as_array());
        for h in hooks.into_iter().flatten() {
            if let Some(c) = h.get("command").and_then(|c| c.as_str())
                && ![Agent::Claude, Agent::Codex]
                    .into_iter()
                    .any(|agent| json_handler_owned(h, agent))
            {
                out.push(c.to_string());
            }
        }
    }
}

/// Unowned commands from either representation of a handler array.
fn toml_other_commands(entry: &dyn TableLike) -> Vec<String> {
    let mut out = Vec::new();
    match entry.get("hooks") {
        Some(Item::ArrayOfTables(a)) => {
            for t in a.iter() {
                if let Some(c) = t.get("command").and_then(|c| c.as_str())
                    && ![Agent::Claude, Agent::Codex]
                        .into_iter()
                        .any(|agent| toml_handler_owned(t, agent))
                {
                    out.push(c.to_string());
                }
            }
        }
        Some(Item::Value(v)) => {
            for t in v
                .as_array()
                .into_iter()
                .flat_map(|a| a.iter())
                .filter_map(|v| v.as_inline_table())
            {
                if let Some(c) = t.get("command").and_then(toml_edit::Value::as_str)
                    && ![Agent::Claude, Agent::Codex]
                        .into_iter()
                        .any(|agent| toml_handler_owned(t, agent))
                {
                    out.push(c.to_string());
                }
            }
        }
        _ => {}
    }
    out
}

fn toml_handler_owned(handler: &dyn TableLike, agent: Agent) -> bool {
    handler.get("type").and_then(Item::as_str) == Some("command")
        && handler
            .get("command")
            .and_then(Item::as_str)
            .is_some_and(|command| owned_command(command, agent))
}

fn toml_is_ours(entry: &dyn TableLike, agent: Agent) -> bool {
    match entry.get("hooks") {
        Some(Item::ArrayOfTables(handlers)) => {
            handlers.iter().any(|h| toml_handler_owned(h, agent))
        }
        Some(Item::Value(v)) => v.as_array().is_some_and(|handlers| {
            handlers.iter().any(|h| {
                h.as_inline_table()
                    .is_some_and(|h| toml_handler_owned(h, agent))
            })
        }),
        _ => false,
    }
}

fn remove_toml_handlers(entry: &mut dyn TableLike, agent: Agent) -> bool {
    let empty = match entry.get_mut("hooks") {
        Some(Item::ArrayOfTables(handlers)) => {
            handlers.retain(|h| !toml_handler_owned(h, agent));
            handlers.is_empty()
        }
        Some(Item::Value(v)) => {
            let Some(handlers) = v.as_array_mut() else {
                return false;
            };
            handlers.retain(|h| {
                !h.as_inline_table()
                    .is_some_and(|h| toml_handler_owned(h, agent))
            });
            handlers.is_empty()
        }
        _ => return false,
    };
    empty
        && (entry.len() == 1
            || (entry.len() == 2 && entry.get("matcher").and_then(Item::as_str) == Some("Bash")))
}

/// Same as [`json_bash_hooks`] for the inline `[[hooks.PreToolUse]]` tables of
/// a Codex `config.toml`.
fn toml_bash_hooks(doc: &DocumentMut, out: &mut Vec<String>) {
    let Some(pre) = doc
        .get("hooks")
        .and_then(|h| h.as_table_like())
        .and_then(|h| h.get("PreToolUse"))
    else {
        return;
    };
    let entries: Vec<&dyn TableLike> = match pre {
        Item::ArrayOfTables(a) => a.iter().map(|t| t as &dyn TableLike).collect(),
        Item::Value(v) => v
            .as_array()
            .into_iter()
            .flat_map(|a| a.iter())
            .filter_map(|v| v.as_inline_table().map(|t| t as &dyn TableLike))
            .collect(),
        _ => Vec::new(),
    };
    for entry in entries {
        let matcher = entry.get("matcher").and_then(|m| m.as_str()).unwrap_or("");
        if !sees_bash(matcher) {
            continue;
        }
        out.extend(toml_other_commands(entry));
    }
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn read_toml(p: &Path) -> Option<DocumentMut> {
    std::fs::read_to_string(p).ok()?.parse().ok()
}

/// Commands of the other PreToolUse hooks that see Bash calls (matcher empty,
/// `*`, or naming `Bash`) in Claude Code's `settings.json` and Codex's
/// `hooks.json` / `config.toml`, for `greeg stats`: when several hooks rewrite
/// the same call, the last `updatedInput` to finish wins and the greeg run
/// never happens.
pub fn other_bash_hooks() -> Vec<String> {
    let mut out = Vec::new();
    if let Some(v) = settings_path().ok().and_then(|p| read_json(&p)) {
        json_bash_hooks(&v, &mut out);
    }
    if let Ok(home) = codex_home() {
        if let Some(v) = read_json(&home.join("hooks.json")) {
            json_bash_hooks(&v, &mut out);
        }
        if let Some(doc) = read_toml(&home.join("config.toml")) {
            toml_bash_hooks(&doc, &mut out);
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|c| seen.insert(c.clone()));
    out
}

pub fn install_claude(uninstall: bool, dry_run: bool) -> Result<()> {
    let sp = settings_path()?;
    let kp = skill_path()?;
    let text = match std::fs::read_to_string(&sp) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {}", sp.display())),
    };
    let (out, had) = edit_claude_config(text.as_deref(), uninstall)
        .with_context(|| format!("edit {}", sp.display()))?;
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
    if out != text.as_deref().unwrap_or("") {
        if let Some(p) = sp.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&sp, out)?;
    }
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
        std::fs::write(&kp, skill_text(Agent::Claude))?;
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
// Codex: `[[hooks.PreToolUse]]` in $CODEX_HOME/config.toml
// ---------------------------------------------------------------------------

const CODEX_HOOK_COMMAND: &str = "greeg hook run --agent codex";

/// The entry as it lands in `config.toml`; also the source of the inline
/// form used when the file keeps `PreToolUse` as an inline array.
const CODEX_HOOK_TOML: &str = "[[hooks.PreToolUse]]
matcher = \"Bash\"

[[hooks.PreToolUse.hooks]]
type = \"command\"
command = \"greeg hook run --agent codex\"
";

/// The largest table position in the document (`None` when nothing is
/// positioned). Tables built in code have no position, and the renderer puts
/// those before every positioned table, splitting an existing array of tables
/// apart; new tables get placed after the last one instead.
fn max_position(item: &Item) -> Option<isize> {
    match item {
        Item::Table(t) => t
            .iter()
            .filter_map(|(_, v)| max_position(v))
            .chain(t.position())
            .max(),
        Item::ArrayOfTables(a) => a
            .iter()
            .filter_map(|t| max_position(&Item::Table(t.clone())))
            .max(),
        _ => None,
    }
}

/// The entry to append: its two tables positioned after `after` and the
/// header preceded by a blank line when the file has content before it.
fn codex_hook_table(after: Option<isize>, blank_line: bool) -> Table {
    let doc: DocumentMut = CODEX_HOOK_TOML.parse().expect("static toml");
    let mut entry = doc["hooks"]["PreToolUse"]
        .as_array_of_tables()
        .and_then(|a| a.get(0))
        .cloned()
        .expect("static toml");
    let base = after.map_or(0, |p| p + 1);
    entry.set_position(Some(base));
    entry
        .decor_mut()
        .set_prefix(if blank_line { "\n" } else { "" });
    if let Some(Item::ArrayOfTables(hs)) = entry.get_mut("hooks") {
        for (i, t) in hs.iter_mut().enumerate() {
            t.set_position(Some(base + 1 + i as isize));
        }
    }
    entry
}

fn codex_hook_inline() -> toml_edit::Value {
    let doc: DocumentMut = format!(
        "x = {{ matcher = \"Bash\", hooks = [{{ type = \"command\", command = \"{CODEX_HOOK_COMMAND}\" }}] }}\n"
    )
    .parse()
    .expect("static toml");
    doc["x"].as_value().cloned().expect("static toml")
}

/// Add (or, with `uninstall`, remove) the greeg entry in the text of a Codex
/// `config.toml`, preserving unrelated handlers, entry metadata, comments and
/// existing `hooks.state` records. Returns the new text and whether
/// the entry was already there. New entries are appended so the positional
/// keys Codex uses for hook trust (`config.toml:pre_tool_use:N:M`) of the
/// other hooks do not shift.
fn edit_codex_config(text: &str, uninstall: bool) -> Result<(String, bool)> {
    let mut doc: DocumentMut = text.parse().context("parse config.toml")?;
    if uninstall && doc.get("hooks").is_none() {
        return Ok((text.to_string(), false));
    }
    let last = max_position(doc.as_item());
    let blank_line = !text.trim().is_empty();
    let hooks = doc.entry("hooks").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    });
    let Some(hooks) = hooks.as_table_like_mut() else {
        bail!("`hooks` in config.toml is not a table; add the hook by hand");
    };
    if uninstall && hooks.get("PreToolUse").is_none() {
        return Ok((text.to_string(), false));
    }
    let pre = hooks
        .entry("PreToolUse")
        .or_insert(Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    let had = match pre {
        Item::ArrayOfTables(arr) => {
            let had = arr.iter().any(|t| {
                toml_is_ours(t, Agent::Codex)
                    && (uninstall || t.get("matcher").and_then(Item::as_str) == Some("Bash"))
            });
            if uninstall {
                for index in (0..arr.len()).rev() {
                    let entry = arr.get_mut(index).unwrap();
                    if toml_is_ours(entry, Agent::Codex)
                        && remove_toml_handlers(entry, Agent::Codex)
                    {
                        arr.remove(index);
                    }
                }
            } else if !had {
                arr.push(codex_hook_table(last, blank_line));
            }
            had
        }
        Item::Value(v) => {
            let Some(arr) = v.as_array_mut() else {
                bail!("`hooks.PreToolUse` in config.toml is not an array; add the hook by hand");
            };
            let had = arr.iter().any(|v| {
                v.as_inline_table().is_some_and(|t| {
                    toml_is_ours(t, Agent::Codex)
                        && (uninstall
                            || t.get("matcher").and_then(toml_edit::Value::as_str) == Some("Bash"))
                })
            });
            if uninstall {
                for index in (0..arr.len()).rev() {
                    let Some(entry) = arr
                        .get_mut(index)
                        .and_then(toml_edit::Value::as_inline_table_mut)
                    else {
                        continue;
                    };
                    if toml_is_ours(entry, Agent::Codex)
                        && remove_toml_handlers(entry, Agent::Codex)
                    {
                        arr.remove(index);
                    }
                }
            } else if !had {
                arr.push(codex_hook_inline());
            }
            had
        }
        _ => bail!("`hooks.PreToolUse` in config.toml is not an array; add the hook by hand"),
    };
    let changed = if uninstall { had } else { !had };
    if !changed {
        return Ok((text.to_owned(), had));
    }
    Ok((doc.to_string(), had))
}

pub fn install_codex(uninstall: bool, dry_run: bool) -> Result<()> {
    let home = codex_home()?;
    let cp = home.join("config.toml");
    let kp = home.join("skills/greeg/SKILL.md");
    let text = match std::fs::read_to_string(&cp) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", cp.display())),
    };
    let (out, had) =
        edit_codex_config(&text, uninstall).with_context(|| format!("edit {}", cp.display()))?;
    if dry_run {
        println!(
            "{}: {}",
            cp.display(),
            if uninstall {
                if had {
                    "would remove the greeg hook"
                } else {
                    "no greeg hook present"
                }
            } else if had {
                "hook already installed"
            } else {
                "would append PreToolUse hook `greeg hook run --agent codex` (matcher Bash)"
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
    if out != text {
        std::fs::create_dir_all(&home)?;
        std::fs::write(&cp, out)?;
    }
    if uninstall {
        let _ = std::fs::remove_file(&kp);
        println!(
            "removed the greeg hook from {} and {}",
            cp.display(),
            kp.display()
        );
    } else {
        if let Some(p) = kp.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&kp, skill_text(Agent::Codex))?;
        println!(
            "{}: {}\n{}: skill written\nstart Codex and run /hooks to trust the new hook (untrusted hooks are skipped); test with: echo '{{\"tool_name\":\"Bash\",\"tool_input\":{{\"command\":\"rg -n foo src\"}}}}' | greeg hook run --agent codex\nnote: PreToolUse hooks run in parallel (the last rewrite to finish wins)",
            cp.display(),
            if had {
                "hook already installed"
            } else {
                "PreToolUse hook `greeg hook run --agent codex` appended"
            },
            kp.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// shell tokenizing
// ---------------------------------------------------------------------------

/// Split a simple command into words (quotes and backslashes honoured).
/// `None` when the shell would expand or interpret something we do not model.
fn shell_words(s: &str) -> Option<Vec<String>> {
    if s.contains('\0') {
        return None;
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
                            if e == '\n' {
                                return None;
                            }
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
                let escaped = chars.next()?;
                if escaped == '\n' {
                    return None;
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
            // redirections, expansions and control operators: not a plain invocation
            '$' | '`' | '<' | '>' | ';' | '&' | '|' | '(' | ')' | '{' | '}' | '\n' => return None,
            // an unquoted glob or `~` would be expanded by the shell: leave it alone
            '*' | '?' | '[' => return None,
            '#' if !in_word => return None,
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
    fn apply(&mut self, kind: Flag, name: &str, value: Option<String>) -> Option<()> {
        match kind {
            Flag::Keep => self.flags.push(name.to_string()),
            Flag::Drop => {}
            Flag::Color => {
                if !matches!(value.as_deref(), Some("never" | "always" | "auto" | "ansi")) {
                    return None;
                }
            }
            Flag::Fixed => self.fixed = true,
            Flag::Unrestricted => self.unrestricted = self.unrestricted.saturating_add(1),
            Flag::Unsupported => return None,
            Flag::Value => {
                let v = value?;
                match name {
                    "-e" | "--regexp" => self.patterns.push(v),
                    "--max-filesize" => {
                        v.parse::<u64>().ok()?;
                        if self.max_filesize.replace(v).is_some() {
                            return None;
                        }
                    }
                    "-A" | "-B" | "-C" | "-j" | "--after-context" | "--before-context"
                    | "--context" | "--threads" | "--max-columns" => {
                        v.parse::<u64>().ok()?;
                        self.flags.push(name.to_string());
                        self.flags.push(v);
                    }
                    "-M" => {
                        v.parse::<u64>().ok()?;
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
        Some(())
    }
}

const VERBS: &[&str] = &[
    "def", "refs", "callers", "impls", "outline", "show", "map", "impact", "index", "doctor",
    "man", "hook", "lang", "stats",
];

fn parse(words: &[String]) -> Option<Parsed> {
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
            if inline.is_some() && !matches!(kind, Flag::Value | Flag::Color) {
                return None;
            }
            let value = match kind {
                Flag::Value | Flag::Color => Some(match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        words.get(i - 1)?.clone()
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
                    words.get(i - 1)?.clone()
                } else {
                    rest.to_string()
                };
                p.apply(kind, &name, Some(value))?;
                break;
            }
            p.apply(kind, &name, None)?;
        }
    }
    Some(p)
}

/// Rewrite a simple rg command to argv: `(original words, greeg words)`;
/// `None` = leave it alone.
fn rewrite_words(seg: &str) -> Option<(Vec<String>, Vec<String>)> {
    let words = shell_words(seg)?;
    let first = words.first()?;
    if first != "rg" {
        return None;
    }
    let mut p = parse(&words)?;
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
    if paths.iter().any(|(w, _)| w == "-") {
        return None;
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
    pub original: Vec<String>,
    pub rewritten: Vec<String>,
}

/// Only complete simple commands qualify; a shell tail may consume another
/// output dialect or change permission and exit-status behavior.
pub fn rewrite_full(cmd: &str) -> Option<Rewrite> {
    let (original, rewritten) = rewrite_words(cmd)?;
    let command = rewritten
        .iter()
        .map(|w| quote(w))
        .collect::<Vec<_>>()
        .join(" ");
    Some(Rewrite {
        command,
        original,
        rewritten,
    })
}

/// The hook's stdout for a rewrite. Codex applies `updatedInput` only next to
/// `permissionDecision: allow` (and rejects a reason without a decision);
/// Claude Code would take that `allow` as skipping the permission prompt, so
/// it gets the reason and the input only.
fn hook_reply(agent: Agent, command: &str) -> Value {
    let mut specific = json!({"hookEventName": "PreToolUse"});
    if agent == Agent::Codex {
        specific["permissionDecision"] = json!("allow");
    }
    specific["permissionDecisionReason"] = json!("greeg rewrite");
    specific["updatedInput"] = json!({"command": command});
    json!({"hookSpecificOutput": specific})
}

pub fn run(agent: Agent) -> Result<()> {
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
    // A configured ripgrep can select another file set or run a preprocessor.
    if std::env::var_os("RIPGREP_CONFIG_PATH").is_some_and(|p| !p.is_empty()) {
        return Ok(());
    }
    if let Some(rw) = rewrite_full(cmd)
        && rw.command != cmd
    {
        let out = hook_reply(agent, &rw.command);
        let mut w = std::io::stdout().lock();
        serde_json::to_writer(&mut w, &out)?;
        writeln!(w)?;
        // Opt-in stats use the unchanged working directory.
        let cwd = v
            .get("cwd")
            .and_then(|c| c.as_str())
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let session = v.get("session_id").and_then(|s| s.as_str());
        crate::stats::record_hook(&cwd, session, &rw.original, &rw.rewritten);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_boundary_newlines_preserve_literal_bytes() {
        for (command, pattern) in [
            ("rg foo\n", "foo"),
            ("\n\trg foo\n \t\n", "foo"),
            ("rg foo\\ \n", "foo "),
            ("rg foo\u{a0}\n", "foo\u{a0}"),
        ] {
            assert_eq!(rewrite_full(command).unwrap().original, ["rg", pattern]);
        }
        for command in ["rg foo\necho tail", "rg foo\\\n", "rg foo\\\n\n"] {
            assert!(rewrite_full(command).is_none(), "{command:?}");
        }
    }

    #[test]
    fn uncertain_commands_are_declined() {
        for cmd in [
            "grep -rl needle .",
            "grep needle src",
            "fgrep needle file.rs",
            "egrep needle file.rs",
            "rg --json needle",
            "rg --stats needle",
            "/usr/bin/rg needle",
            "./rg needle",
            "rg needle # comment",
            "rg needle\\\n src",
            "rg \"nee\\\ndle\" src",
            "rg -g*.rs needle",
            "rg needle | head",
            "rg -l needle | xargs echo",
            "rg needle && echo yes",
            "rg needle || echo no",
            "rg needle; echo done",
            "rg needle &",
            "cd src && rg needle",
            "rg needle\necho done",
            "rg -h needle",
            "rg --ignore-case=false needle",
            "rg --fixed-strings=no needle",
        ] {
            assert!(rewrite_full(cmd).is_none(), "unexpected rewrite: {cmd:?}");
        }
    }

    #[test]
    fn hooks_request_exact_matching() {
        let r = rewrite_full("rg -w NEEDLE src").unwrap();
        assert_eq!(
            r.command,
            "greeg --matching exact --max-filesize 18446744073709551615 -w NEEDLE src"
        );
    }

    #[track_caller]
    fn same(cmd: &str, want: &str) {
        assert_eq!(
            rewrite_full(cmd).map(|r| r.command).as_deref(),
            Some(want),
            "{cmd}"
        );
    }

    #[track_caller]
    fn untouched(cmd: &str) {
        assert!(rewrite_full(cmd).is_none(), "unexpected rewrite: {cmd:?}");
    }

    #[test]
    fn supported_flags_and_positionals() {
        for (cmd, args) in [
            (
                "rg -in 'get queryset' --type py",
                "-i --type py 'get queryset'",
            ),
            ("rg def --type rs", "--type rs -e def"),
            ("rg stats src", "-e stats src"),
            ("rg -C 3 'fn main' crates", "-C 3 'fn main' crates"),
            ("rg foo . ../other", "foo . ../other"),
            ("rg foo -- -weird", "foo -- -weird"),
            ("rg foo \"a b\"", "foo 'a b'"),
            ("rg -e foo bar src", "foo bar src"),
            ("rg -- -x src", "-- -x src"),
            ("rg -F -- --flag", "-F -- --flag"),
            ("rg -e -x src", "-- -x src"),
            ("rg -- def src", "-- def src"),
            ("rg -tjs foo", "-t js foo"),
            ("rg -gX foo", "-g X foo"),
            ("rg -eX src", "X src"),
            ("rg -A3 foo", "-A 3 foo"),
            ("rg -B2 foo", "-B 2 foo"),
            ("rg -C1 foo", "-C 1 foo"),
            ("rg -iw foo", "-i -w foo"),
            ("rg -itrs foo", "-i -t rs foo"),
            ("rg -inA 2 foo src", "-i -A 2 foo src"),
            ("rg -M 80 foo", "--max-columns 80 foo"),
            ("rg -j4 foo", "-j 4 foo"),
            ("rg -l foo", "-l foo"),
            ("rg -c foo src", "-c foo src"),
            ("rg -x foo", "-x foo"),
            ("rg -U 'a\\nb' src", "-U 'a\\nb' src"),
            ("rg -u foo", "--no-ignore foo"),
            ("rg -uu foo", "--no-ignore --hidden foo"),
            ("rg --unrestricted -i foo", "-i --no-ignore foo"),
        ] {
            same(
                cmd,
                &format!("greeg --matching exact --max-filesize 18446744073709551615 {args}"),
            );
        }
    }

    #[test]
    fn ranked_presentation_flags() {
        for flags in [
            "-nH",
            "-N",
            "--no-heading --heading",
            "--color never",
            "--color=never",
            "-p --column --no-column",
            "--trim --line-buffered",
        ] {
            same(
                &format!("rg {flags} foo"),
                "greeg --matching exact --max-filesize 18446744073709551615 foo",
            );
        }
    }

    #[test]
    fn unsupported_flags_and_syntax() {
        for cmd in [
            "git status",
            "rg -e a -e b",
            "rg -Cx foo",
            "rg -iv foo",
            "rg -uuu foo",
            "rg -u -u -u foo",
            "rg -v foo",
            "rg -o x src",
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
            "rg --pre=./script foo",
            "rg --pre-glob '*.pdf' --pre cat foo",
            "rg --search-zip foo",
            "rg -a foo",
            "rg --iglob '*.rs' foo",
            "rg -E utf-16 foo",
            "rg -f patterns.txt src",
            "rg --sort path foo",
            "rg --sortr=modified foo",
            "rg --no-messages foo",
            "rg --vimgrep foo",
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
            "rg foo src/[ab]",
            "rg foo $DIR",
            "rg foo \"$DIR\"",
            "rg foo `pwd`",
            "rg foo ~/src",
            "(rg foo)",
            "rg foo {a,b}",
            "rg foo -",
            "rg foo 'unterminated",
            "rg foo\0bar",
            "rg",
            "rg --glob",
            "rg --glob='*.rs' --ignore-case=yes needle",
            "rg --count=false needle",
        ] {
            untouched(cmd);
        }
        untouched(&format!("rg -{} foo", "u".repeat(256)));
    }

    #[test]
    fn quoted_metacharacters_remain_arguments() {
        for (cmd, pattern) in [
            ("rg 'a*b' src", "a*b"),
            ("rg 'a|b' src", "a|b"),
            ("rg '#' src", "#"),
            ("rg \\# src", "#"),
            ("rg a#b src", "a#b"),
            ("rg '; echo tail' src", "; echo tail"),
            ("rg '$(pwd)' src", "$(pwd)"),
            ("rg '' src", ""),
            ("rg 'a'\\''b' src", "a'b"),
        ] {
            let r = rewrite_full(cmd).unwrap();
            assert_eq!(r.original, ["rg", pattern, "src"]);
            assert_eq!(
                r.rewritten,
                [
                    "greeg",
                    "--matching",
                    "exact",
                    "--max-filesize",
                    "18446744073709551615",
                    pattern,
                    "src"
                ]
            );
            assert_eq!(shell_words(&r.command).unwrap(), r.rewritten);
        }
        same(
            "rg -g '*.rs' foo",
            "greeg --matching exact --max-filesize 18446744073709551615 -g '*.rs' foo",
        );
    }

    #[test]
    fn hook_reply_shapes() {
        let claude = hook_reply(Agent::Claude, "greeg foo");
        assert_eq!(
            claude,
            json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecisionReason": "greeg rewrite", "updatedInput": {"command": "greeg foo"}}})
        );
        let codex = hook_reply(Agent::Codex, "greeg foo");
        assert_eq!(
            codex["hookSpecificOutput"]["permissionDecision"],
            json!("allow")
        );
        assert_eq!(
            codex["hookSpecificOutput"]["updatedInput"]["command"],
            json!("greeg foo")
        );
    }

    #[test]
    fn skill_notes_per_agent() {
        let claude = skill_text(Agent::Claude);
        let codex = skill_text(Agent::Codex);
        assert!(claude.starts_with("---\nname: greeg\n"));
        assert!(claude.contains("Bash(greeg:*)"));
        assert!(!codex.contains("Bash(greeg:*)"));
        assert!(codex.contains("greeg hook run --agent codex"));
        assert!(codex.contains("greeg impact NAME"));
    }

    const CODEX_CONFIG: &str = r#"model = "gpt-6-astra"   # pinned

[features]
hooks = true

[[hooks.PreToolUse]]
matcher = "Bash"

[[hooks.PreToolUse.hooks]]
command = "/opt/git-ai checkpoint codex --hook-input stdin"
type = "command"

[[hooks.PreToolUse]]
matcher = "apply_patch"

[[hooks.PreToolUse.hooks]]
command = "/opt/patch-linter"
type = "command"

[hooks.state."/home/u/.codex/config.toml:pre_tool_use:0:0"]
enabled = true
trusted_hash = "sha256:abc"

[mcp_servers.context7]
args = ["-y", "@upstash/context7-mcp"]
"#;

    #[test]
    fn codex_config_empty_file() {
        let (out, had) = edit_codex_config("", false).unwrap();
        assert!(!had);
        assert_eq!(out, CODEX_HOOK_TOML);
        let (again, had) = edit_codex_config(&out, false).unwrap();
        assert!(had);
        assert_eq!(again, out);
        let (removed, had) = edit_codex_config(&out, true).unwrap();
        assert!(had);
        assert_eq!(removed.trim(), "");
        let (untouched, had) = edit_codex_config("", true).unwrap();
        assert!(!had);
        assert_eq!(untouched, "");
    }

    #[test]
    fn codex_config_appends_and_preserves() {
        let (out, had) = edit_codex_config(CODEX_CONFIG, false).unwrap();
        assert!(!had);
        assert_eq!(out, format!("{CODEX_CONFIG}\n{CODEX_HOOK_TOML}"));
        let doc: DocumentMut = out.parse().unwrap();
        let pre = doc["hooks"]["PreToolUse"].as_array_of_tables().unwrap();
        assert_eq!(pre.len(), 3);
        assert!(toml_is_ours(pre.get(2).unwrap(), Agent::Codex));
        let mut others = Vec::new();
        toml_bash_hooks(&doc, &mut others);
        assert_eq!(
            others,
            vec!["/opt/git-ai checkpoint codex --hook-input stdin".to_string()]
        );
        let (back, had) = edit_codex_config(&out, true).unwrap();
        assert!(had);
        assert_eq!(back, CODEX_CONFIG);
    }

    #[test]
    fn codex_config_inline_array() {
        let cfg = "[hooks]\nPreToolUse = [{ matcher = \"Bash\", hooks = [{ type = \"command\", command = \"/opt/x\" }] }]\n";
        let (out, had) = edit_codex_config(cfg, false).unwrap();
        assert!(!had);
        let doc: DocumentMut = out.parse().unwrap();
        let arr = doc["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        let mut others = Vec::new();
        toml_bash_hooks(&doc, &mut others);
        assert_eq!(others, vec!["/opt/x".to_string()]);
        let (back, had) = edit_codex_config(&out, true).unwrap();
        assert!(had);
        assert_eq!(back, cfg);
        let bad = "hooks = 3\n";
        assert!(edit_codex_config(bad, false).is_err());
    }

    #[test]
    fn json_hooks_filter() {
        let v = json!({"hooks": {"PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "greeg hook run"}, {"type": "command", "command": "rtk hook claude"}]},
            {"matcher": "Edit", "hooks": [{"type": "command", "command": "fmt"}]},
            {"hooks": [{"type": "command", "command": "audit"}]},
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "greeg hook-helper"}, {"type": "prompt", "command": "greeg hook run"}]}
        ]}});
        let mut out = Vec::new();
        json_bash_hooks(&v, &mut out);
        assert_eq!(
            out,
            vec![
                "rtk hook claude".to_string(),
                "audit".to_string(),
                "greeg hook-helper".to_string(),
                "greeg hook run".to_string()
            ]
        );
    }

    #[test]
    fn hook_ownership_requires_exact_command_and_type() {
        for agent in [Agent::Claude, Agent::Codex] {
            let command = match agent {
                Agent::Claude => "greeg hook run",
                Agent::Codex => CODEX_HOOK_COMMAND,
            };
            assert!(json_handler_owned(
                &json!({"type": "command", "command": command}),
                agent
            ));
            assert!(!json_handler_owned(
                &json!({"type": "prompt", "command": command}),
                agent
            ));
            assert!(!json_handler_owned(&json!({"command": command}), agent));
            for command in [
                "greeg hook-helper",
                "greeg hook run-extra",
                "greeg hook run && audit",
                "/opt/greeg hook run",
                "greeg hook run --custom",
            ] {
                let handler = json!({"type": "command", "command": command});
                assert!(!json_handler_owned(&handler, agent), "{command}");
            }
        }
        assert!(!owned_command(CODEX_HOOK_COMMAND, Agent::Claude));
        assert!(!owned_command("greeg hook run", Agent::Codex));
    }

    #[test]
    fn claude_config_edits_preserve_custom_handlers_and_metadata() {
        let input = json!({"hooks": {"PostToolUse": [{"hooks": [{"command": "audit"}]}],
            "PreToolUse": [
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "greeg hook-helper"}]},
                {"matcher": "Bash", "label": "keep", "hooks": [{"type": "command", "command": "greeg hook run"}]},
                {"matcher": "Bash", "hooks": [
                    {"type": "command", "command": "greeg hook run"},
                    {"type": "command", "command": "greeg hook run --agent claude"},
                    {"type": "prompt", "command": "greeg hook run"},
                    {"type": "command", "command": "greeg hook run --agent codex"}
                ]}
            ]}}).to_string();
        let (out, had) = edit_claude_config(Some(&input), true).unwrap();
        assert!(had);
        let value: Value = serde_json::from_str(&out).unwrap();
        let original: Value = serde_json::from_str(&input).unwrap();
        assert_eq!(
            value["hooks"]["PostToolUse"],
            original["hooks"]["PostToolUse"]
        );
        let entries = value["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0], original["hooks"]["PreToolUse"][0]);
        assert_eq!(
            entries[1],
            json!({"matcher": "Bash", "label": "keep", "hooks": []})
        );
        assert_eq!(entries[2]["hooks"].as_array().unwrap().len(), 2);
        let (again, had) = edit_claude_config(Some(&out), true).unwrap();
        assert!(!had);
        assert_eq!(again, out);
        let (installed, had) = edit_claude_config(Some(&out), false).unwrap();
        assert!(!had);
        let installed: Value = serde_json::from_str(&installed).unwrap();
        assert_eq!(installed["hooks"]["PreToolUse"][0], entries[0]);
        assert_eq!(installed["hooks"]["PreToolUse"][3], hook_entry());
    }

    #[test]
    fn codex_shared_handlers_support_all_array_forms() {
        let bodies = [
            "[[hooks.PreToolUse]]\nmatcher = 'Bash'\nlabel = 'keep'\n[[hooks.PreToolUse.hooks]]\ntype = 'command'\ncommand = 'greeg hook run --agent codex'\n[[hooks.PreToolUse.hooks]]\n# keep handler\ntype = 'command'\ncommand = 'audit'\ntimeout = 7\n",
            "[[hooks.PreToolUse]]\nmatcher = 'Bash'\nlabel = 'keep'\nhooks = [{type='command', command='greeg hook run --agent codex'}, {type='command', command='audit', timeout=7}]\n",
            "[hooks]\nPreToolUse = [{matcher='Bash', label='keep', hooks=[{type='command', command='greeg hook run --agent codex'}, {type='command', command='audit', timeout=7}]}]\n",
        ];
        for body in bodies {
            let state = "\n[hooks.state.'config.toml:pre_tool_use:0:1']\ntrusted_hash = 'sha256:keep'\nenabled = true\n";
            let input = format!("# user comment\n{body}{state}");
            let (out, had) = edit_codex_config(&input, true).unwrap();
            assert!(had);
            assert!(!out.contains(CODEX_HOOK_COMMAND));
            assert!(out.contains("label='keep'") || out.contains("label = 'keep'"));
            assert!(out.contains("timeout=7") || out.contains("timeout = 7"));
            assert!(out.contains(state));
            assert!(out.contains("# user comment"));
            let doc: DocumentMut = out.parse().unwrap();
            let mut other = Vec::new();
            toml_bash_hooks(&doc, &mut other);
            assert_eq!(other, ["audit"]);
            let (again, had) = edit_codex_config(&out, true).unwrap();
            assert!(!had);
            assert_eq!(again, out);
        }
    }

    #[test]
    fn claude_uninstall_removes_only_empty_default_entries() {
        for metadata in [
            json!({}),
            json!({"matcher": "Bash"}),
            json!({"matcher": "Edit"}),
            json!({"label": "keep"}),
        ] {
            let mut entry = metadata.clone();
            entry["hooks"] = json!([{"type": "command", "command": "greeg hook run"}]);
            let input = json!({"hooks": {"PreToolUse": [entry, {"hooks": []}]}}).to_string();
            let (out, had) = edit_claude_config(Some(&input), true).unwrap();
            assert!(had);
            let value: Value = serde_json::from_str(&out).unwrap();
            let expected = if metadata == json!({}) || metadata == json!({"matcher": "Bash"}) {
                json!([{"hooks": []}])
            } else {
                let mut preserved = metadata;
                preserved["hooks"] = json!([]);
                json!([preserved, {"hooks": []}])
            };
            assert_eq!(value["hooks"]["PreToolUse"], expected);
        }
    }

    #[test]
    fn codex_uninstall_removes_only_empty_default_entries() {
        for metadata in ["", "matcher='Bash'", "matcher='Edit'", "label='keep'"] {
            let handler = "{type='command',command='greeg hook run --agent codex'}";
            let bodies = [
                format!(
                    "[[hooks.PreToolUse]]\n{metadata}\n[[hooks.PreToolUse.hooks]]\ntype='command'\ncommand='greeg hook run --agent codex'\n[[hooks.PreToolUse]]\nhooks=[]\n"
                ),
                format!(
                    "[[hooks.PreToolUse]]\n{metadata}\nhooks=[{handler}]\n[[hooks.PreToolUse]]\nhooks=[]\n"
                ),
                format!(
                    "[hooks]\nPreToolUse=[{{{}hooks=[{handler}]}},{{hooks=[]}}]\n",
                    if metadata.is_empty() {
                        String::new()
                    } else {
                        format!("{metadata},")
                    }
                ),
            ];
            for input in bodies {
                let (out, had) = edit_codex_config(&input, true).unwrap();
                assert!(had);
                let doc: DocumentMut = out.parse().unwrap();
                let pre = &doc["hooks"]["PreToolUse"];
                let entries = pre
                    .as_array_of_tables()
                    .map(|a| a.len())
                    .or_else(|| pre.as_array().map(|a| a.len()))
                    .unwrap();
                let remove = metadata.is_empty() || metadata == "matcher='Bash'";
                assert_eq!(entries, if remove { 1 } else { 2 }, "{input}");
                assert!(!out.contains(CODEX_HOOK_COMMAND));
                if !remove {
                    assert!(out.contains(metadata));
                }
                assert_eq!(edit_codex_config(&out, true).unwrap(), (out, false));
            }
        }
    }

    #[test]
    fn config_noops_preserve_original_text() {
        for text in [
            "{   }\n",
            "{\"hooks\": {\"PostToolUse\": []}}",
            "{\"hooks\":{\"PreToolUse\":[{\"matcher\":\"Bash\",\"hooks\":[{\"type\":\"command\",\"command\":\"greeg hook-helper\"}]}]}}",
        ] {
            let (out, had) = edit_claude_config(Some(text), true).unwrap();
            assert!(!had);
            assert_eq!(out, text);
        }
        let (text, _) = edit_claude_config(None, false).unwrap();
        assert_eq!(
            edit_claude_config(Some(&text), false).unwrap(),
            (text, true)
        );
        assert_eq!(
            edit_claude_config(None, true).unwrap(),
            (String::new(), false)
        );
        assert!(edit_claude_config(Some(""), true).is_err());
        for text in [
            "# untouched\n[hooks]\nPreToolUse = []\n",
            "[hooks]\nPreToolUse=[{matcher='Bash',hooks=[{type='command',command='greeg hook-helper'}]}]\n",
        ] {
            assert_eq!(
                edit_codex_config(text, true).unwrap(),
                (text.to_owned(), false)
            );
        }
    }

    #[test]
    fn installation_does_not_mistake_another_matcher_for_bash() {
        let input = json!({"hooks":{"PreToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"greeg hook run"}]}]}}).to_string();
        let (out, had) = edit_claude_config(Some(&input), false).unwrap();
        assert!(!had);
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        let input = CODEX_HOOK_TOML.replace("matcher = \"Bash\"", "matcher = \"Edit\"");
        let (out, had) = edit_codex_config(&input, false).unwrap();
        assert!(!had);
        let doc: DocumentMut = out.parse().unwrap();
        assert_eq!(
            doc["hooks"]["PreToolUse"]
                .as_array_of_tables()
                .unwrap()
                .len(),
            2
        );
    }
}
