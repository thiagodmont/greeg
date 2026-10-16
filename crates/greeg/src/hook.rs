//! `greeg hook claude` / `greeg hook codex`: install a PreToolUse hook that
//! rewrites simple `rg` Bash calls to `greeg`, plus a short skill file.
//! `greeg hook run [--agent claude|codex]` is the hook: it reads the tool call
//! JSON on stdin and prints an `updatedInput` when the command is a plain
//! rg invocation it can map. Both agents send the same input shape
//! (`tool_name`, `tool_input.command`, `cwd`, `session_id`); the reply differs
//! only in that Codex applies a rewrite solely when it comes with
//! `permissionDecision: allow`, which Claude Code would read as an auto-allow.
//!
//! `rg` and recursive `grep` commands qualify, alone, in a `;`/`&&`/`||`
//! list or piped into `head`. Other greps, pipelines, subshells, explicit executable paths and
//! shell expansion stay with the original tool.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table, TableLike};

use crate::rewrite::{Rewrite, rewrite_full};

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

`greeg PATTERN [PATHS]` accepts ripgrep flags (`-i -w -F -t rs -g '*.py' -A/-B/-C`)
and returns hits classified as def/import/call/type/member/ident/doc/comment/string with
the enclosing symbol, tests and vendored code demoted, within a token budget
(`--budget N`, or `low`/`medium`/`high`/`none`; default 2000 or the user's
`greeg budget` level). Broad queries return facets first; the footer says what
was cut and suggests the next query. Exit 1 means no exact hits, even if the
discovery ladder found word-boundary, case, split-token or fuzzy-name suggestions.

`-l` prints only paths and `-c` prints `path:count`, one per line on stdout, so both
use the machine output modes. Filenames are newline-delimited, not NUL-delimited.
For machine-readable output use `--json=greeg`: typed JSON Lines records (searches and
the symbol verbs below) whose last record, the footer, holds the `outcome` (exit, total,
shown, complete). `--budget` counts the JSON's own tokens, so it holds fewer rows than
text. `--json=rg` prints exactly ripgrep's records.

File searches with `-l`, `-c`, `--mode files|count`, `--budget 0` or JSON
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
    greeg impact NAME           files using NAME: likely affected / possible / review

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

The installed PreToolUse hook (`greeg hook run`) rewrites `rg` and `grep -r`
Bash calls into `greeg --matching exact`, also each one in a `;`/`&&`/`||`
list or piped into `head -N` (head then shows greeg's best lines), with a
trailing `2>/dev/null`, `2>&1` or `</dev/null` kept. A
rewritten `grep -r` skips ignored, hidden and binary files, as rg does, and
says so on stderr (`-uu` searches ignored and hidden files). Other greps,
JSON, other pipes, subshells, comments, expansions and explicit executable
paths stay unchanged. PreToolUse hooks run in parallel and the last `updatedInput`
wins, so another Bash rewriter (for example RTK) can override or be overridden by
this one; position in `settings.json` does not order them. Permission allow rules
such as `Bash(rg:*)` no longer match the rewritten command: pair them with
`Bash(greeg:*)`.
";

const CODEX_HOOK_NOTES: &str = "## Hook notes

The installed PreToolUse hook (`greeg hook run --agent codex`) rewrites `rg`
and `grep -r` shell calls into `greeg --matching exact`, also one piped into
`head -N` (head then shows greeg's best lines), with a trailing
`2>/dev/null`, `2>&1` or `</dev/null` kept, and a `;`/`&&`/`||` list of
them only (a list with any other command stays unchanged). A rewritten
`grep -r` skips ignored, hidden and binary files, as rg does, and says so on
stderr (`-uu` searches ignored and hidden files). Other greps, JSON, other
pipes, subshells, comments, expansions and explicit executable paths
stay unchanged. PreToolUse hooks run in parallel and the
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
    let config = crate::hook_config::Config::read(&sp)?;
    let (out, had) = edit_claude_config(config.text(), uninstall)
        .with_context(|| format!("edit {}", sp.display()))?;
    let skill =
        crate::hook_skill::Skill::read(&kp, &skill_text(Agent::Claude), Agent::Claude, uninstall)?;
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
        println!("{}: {}", kp.display(), skill.description(true));
        return Ok(());
    }
    config.write(&out)?;
    skill.apply()?;
    if uninstall {
        println!(
            "{}: {}\n{}: {}",
            sp.display(),
            if had {
                "greeg hook removed"
            } else {
                "no greeg hook present"
            },
            kp.display(),
            skill.description(false)
        );
    } else {
        println!(
            "{}: {}\n{}: {}\nrestart Claude Code (or /hooks) to pick up the hook; test with: echo '{{\"tool_name\":\"Bash\",\"tool_input\":{{\"command\":\"rg -n foo src\"}}}}' | greeg hook run\nnote: PreToolUse hooks run in parallel (last updatedInput wins); pair Bash(rg:*) allow rules with Bash(greeg:*)",
            sp.display(),
            if had {
                "hook already installed"
            } else {
                "PreToolUse hook `greeg hook run` added"
            },
            kp.display(),
            skill.description(false)
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
    let config = crate::hook_config::Config::read(&cp)?;
    let (out, had) = edit_codex_config(config.text().unwrap_or(""), uninstall)
        .with_context(|| format!("edit {}", cp.display()))?;
    let skill =
        crate::hook_skill::Skill::read(&kp, &skill_text(Agent::Codex), Agent::Codex, uninstall)?;
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
        println!("{}: {}", kp.display(), skill.description(true));
        return Ok(());
    }
    config.write(&out)?;
    skill.apply()?;
    if uninstall {
        println!(
            "{}: {}\n{}: {}",
            cp.display(),
            if had {
                "greeg hook removed"
            } else {
                "no greeg hook present"
            },
            kp.display(),
            skill.description(false)
        );
    } else {
        println!(
            "{}: {}\n{}: {}\nstart Codex and run /hooks to trust the new hook (untrusted hooks are skipped); test with: echo '{{\"tool_name\":\"Bash\",\"tool_input\":{{\"command\":\"rg -n foo src\"}}}}' | greeg hook run --agent codex\nnote: PreToolUse hooks run in parallel (the last rewrite to finish wins)",
            cp.display(),
            if had {
                "hook already installed"
            } else {
                "PreToolUse hook `greeg hook run --agent codex` appended"
            },
            kp.display(),
            skill.description(false)
        );
    }
    Ok(())
}

/// The hook's stdout for a rewrite. Codex applies `updatedInput` only next to
/// `permissionDecision: allow` (and rejects a reason without a decision);
/// Claude Code would take that `allow` as skipping the permission prompt, so
/// it gets the reason and the input only.
/// The rewrite reply. `updatedInput` is the call's `tool_input` with only
/// `command` replaced, so its other fields (timeout, background, description)
/// survive whether the host merges the update or replaces the input.
fn hook_reply(agent: Agent, tool_input: &Value, command: &str) -> Value {
    let mut specific = json!({"hookEventName": "PreToolUse"});
    if agent == Agent::Codex {
        specific["permissionDecision"] = json!("allow");
    }
    specific["permissionDecisionReason"] = json!("greeg rewrite");
    let mut updated = match tool_input {
        Value::Object(_) => tool_input.clone(),
        _ => json!({}),
    };
    updated["command"] = json!(command);
    specific["updatedInput"] = updated;
    json!({"hookSpecificOutput": specific})
}

/// What the hook does with `cmd` in this environment: the rewrite, or why
/// the original command runs unchanged.
fn decide(cmd: &str, agent: Agent, cwd: &Path) -> Result<Rewrite, String> {
    // A configured ripgrep can select another file set or run a preprocessor.
    if std::env::var_os("RIPGREP_CONFIG_PATH").is_some_and(|p| !p.is_empty()) {
        return Err("RIPGREP_CONFIG_PATH is set (ripgrep configuration is not interpreted)".into());
    }
    let rw = rewrite_full(cmd)?;
    if let Some(r) = rw
        .parts
        .iter()
        .filter_map(|p| p.regex.as_deref())
        .find(|r| !greeg_query::pattern_compiles(r))
    {
        return Err(format!(
            "the grep pattern translates to `{r}`, which greeg's regex does not take"
        ));
    }
    if rw.parts.iter().any(|p| p.unquoted_glob) && !globs_stay_literal(cwd) {
        return Err(
            "the shell would expand an unquoted `--include=`/`--exclude=` glob here".into(),
        );
    }
    // Codex runs an allowed rewrite without asking: it may hold only commands
    // the hook wrote
    if agent == Agent::Codex && !rw.whole {
        return Err(
            "Codex approves a rewrite without asking; a list with other commands runs unchanged"
                .into(),
        );
    }
    Ok(rw)
}

/// No name in `dir` starts as an unquoted `--include=`-style glob does, so
/// the shell passes the glob on as it is (bash; zsh stops at the unmatched
/// glob, so the original command would fail).
fn globs_stay_literal(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().all(|e| {
        let name = e.file_name();
        let name = name.as_encoded_bytes();
        !(name.starts_with(b"--include=")
            || name.starts_with(b"--exclude=")
            || name.starts_with(b"--exclude-dir="))
    })
}

/// `greeg hook explain COMMAND`: the command the hook would run instead
/// (exit 0), or `declined: REASON` (exit 1).
pub fn explain(cmd: &str, agent: Agent) -> Result<bool> {
    let mut w = std::io::stdout().lock();
    let cwd = std::env::current_dir().unwrap_or_default();
    match decide(cmd, agent, &cwd) {
        Ok(rw) => {
            writeln!(w, "{}", rw.command)?;
            Ok(true)
        }
        Err(why) => {
            writeln!(w, "declined: {why}")?;
            Ok(false)
        }
    }
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
    let Some(tool_input) = v.get("tool_input") else {
        return Ok(());
    };
    let Some(cmd) = tool_input.get("command").and_then(|c| c.as_str()) else {
        return Ok(());
    };
    // the directory the command starts in; a `cd` in the line may leave it
    let cwd = v
        .get("cwd")
        .and_then(|c| c.as_str())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    if let Ok(rw) = decide(cmd, agent, &cwd) {
        let out = hook_reply(agent, tool_input, &rw.command);
        let mut w = std::io::stdout().lock();
        serde_json::to_writer(&mut w, &out)?;
        writeln!(w)?;
        let session = v.get("session_id").and_then(|s| s.as_str());
        for p in rw.parts.iter().filter(|p| !p.after_other && !p.piped) {
            crate::stats::record_hook(&cwd, session, &p.original, &p.rewritten);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewrite::shell_words;

    #[test]
    fn command_boundary_newlines_preserve_literal_bytes() {
        for (command, pattern) in [
            ("rg foo\n", "foo"),
            ("\n\trg foo\n \t\n", "foo"),
            ("rg foo\\ \n", "foo "),
            ("rg foo\u{a0}\n", "foo\u{a0}"),
        ] {
            assert_eq!(
                rewrite_full(command).unwrap().parts[0].original,
                ["rg", pattern]
            );
        }
        for command in ["rg foo\\\n", "rg foo\\\n\n"] {
            assert!(rewrite_full(command).is_err(), "{command:?}");
        }
    }

    #[test]
    fn uncertain_commands_are_declined() {
        for cmd in [
            "grep -rc needle .",
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
            "rg needle | head -c 5",
            "rg -l needle | xargs echo",
            "rg needle &",
            "rg -h needle",
            "rg --ignore-case=false needle",
            "rg --fixed-strings=no needle",
        ] {
            assert!(rewrite_full(cmd).is_err(), "unexpected rewrite: {cmd:?}");
        }
    }

    #[test]
    fn declined_commands_name_the_reason() {
        for (cmd, why) in [
            ("grep needle src", "grep without -r"),
            ("grep -rP 'a\\d' .", "unsupported grep flag `-P`"),
            ("grep -r '\\(a\\)\\1' .", "back-reference"),
            ("grep -R needle .", "follows symlinks"),
            ("grep -rc needle .", "without a match"),
            (
                "grep -r --include='src/*.rs' needle .",
                "no glob equivalent",
            ),
            ("./rg needle", "explicit executable path"),
            ("FOO=1 rg foo", "environment assignment"),
            ("git status", "not an rg or grep command"),
            ("rg needle # comment", "shell comment"),
            (
                "rg needle | sort",
                "pipeline (only `| head [-n N]` is kept)",
            ),
            ("rg needle &", "background job"),
            ("echo a; git status", "not an rg or grep command"),
            ("git status; rg --pre=x a", "unsupported flag `--pre`"),
            ("rg foo > out.txt", "redirection"),
            ("rg foo $DIR", "shell expansion"),
            ("rg foo src/*.rs", "unquoted glob"),
            ("rg foo ~/src", "tilde expansion"),
            ("rg foo 'unterminated", "unterminated quote"),
            ("rg needle\\\n src", "line continuation"),
            ("rg --pre=./script foo", "unsupported flag `--pre`"),
            ("rg -iv foo", "unsupported flag `-v`"),
            ("rg --count=false needle", "`--count` does not take a value"),
            ("rg -A x foo", "`-A` needs a whole number"),
            ("rg --glob", "`--glob` needs a value"),
            ("rg --max-filesize 1 --max-filesize 2 foo", "given twice"),
            ("rg -e a -e b", "several -e patterns"),
            ("rg", "no pattern"),
            ("rg foo -", "stdin search"),
            ("rg -uuu foo", "binary files"),
        ] {
            let got = rewrite_full(cmd).err().unwrap_or_default();
            assert!(got.contains(why), "{cmd:?}: {got:?}");
        }
    }

    #[test]
    fn lists_rewrite_each_rg_command_and_keep_every_other_byte() {
        let g = "greeg --matching exact --max-filesize 18446744073709551615";
        for (cmd, want) in [
            ("rg needle && echo yes", format!("{g} needle && echo yes")),
            ("cd src && rg needle", format!("cd src && {g} needle")),
            ("rg a; rg -l b src", format!("{g} a; {g} -l b src")),
            ("rg needle\necho done", format!("{g} needle\necho done")),
            ("rg --json a ; rg b", format!("rg --json a ; {g} b")),
            (
                "echo $HOME;  rg needle  ",
                format!("echo $HOME;  {g} needle  "),
            ),
            ("rg 'a;b' || echo none", format!("{g} 'a;b' || echo none")),
            ("rg x 2>&1; rg y", format!("{g} x 2>&1; {g} y")),
            ("rg x 2>err; rg y", format!("rg x 2>err; {g} y")),
            ("rg a | sort; rg b", format!("rg a | sort; {g} b")),
        ] {
            let r = rewrite_full(cmd).unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
            assert_eq!(r.command, want, "{cmd:?}");
            for p in &r.parts {
                assert_eq!(shell_words(&p.command).unwrap(), p.rewritten, "{cmd:?}");
            }
        }
        for (cmd, why) in [
            ("rg a & rg b", "background job"),
            ("(rg a); rg b", "subshell"),
            ("rg a; echo $(date)", "command substitution"),
            ("rg a; echo \"`date`\"", "command substitution"),
            ("rg a # c; rg b", "shell comment"),
            ("rg a; cat <<EOF", "here-document"),
            ("rg a; echo 'open", "unterminated quote"),
            ("rg x 2>err; echo done", "redirection"),
            ("rg a; echo $'it\\'s'; rg b", "ANSI-C or locale quoting"),
            ("rg a; echo $\"x\"", "ANSI-C or locale quoting"),
            ("[[ -d src && -f x ]] && rg a", "conditional expression"),
            ("rg a; echo x \\\\ #; rg b", "shell comment"),
            (
                "export RIPGREP_CONFIG_PATH=x; rg a",
                "change the environment",
            ),
            ("RIPGREP_CONFIG_PATH=x; rg a", "change the environment"),
            ("rg a && source env.sh && rg b", "change the environment"),
            ("alias rg='rg -i'; rg a", "change the environment"),
            ("set -o noglob; rg a", "change the environment"),
        ] {
            let got = rewrite_full(cmd).err().unwrap_or_default();
            assert!(got.contains(why), "{cmd:?}: {got:?}");
        }
    }

    #[test]
    fn a_list_keeps_what_the_shell_reads_as_one_word() {
        let g = "greeg --matching exact --max-filesize 18446744073709551615";
        // an escaped `#` and a `#` after an escaped blank are literal
        same("rg a\\ #b; rg c", &format!("{g} 'a #b'; {g} c"));
        same("echo x\\#; rg c", &format!("echo x\\#; {g} c"));
        // an escaped trailing blank is part of the pattern
        same("rg a\\ ; rg b", &format!("{g} 'a '; {g} b"));
        same("rg a\\ ", &format!("{g} 'a '"));
        // a trailing `;` or newline ends the only command
        same("rg a;", &format!("{g} a;"));
        same("rg a ;\n", &format!("{g} a ;\n"));
        // the environment of a command alone is its own decline reason
        let why = rewrite_full("RIPGREP_CONFIG_PATH=x rg a")
            .err()
            .unwrap_or_default();
        assert!(why.contains("environment assignment"), "{why}");
    }

    #[test]
    fn a_search_piped_into_head_keeps_the_head() {
        let g = "greeg --matching exact --max-filesize 18446744073709551615";
        for head in [
            "head",
            "head -5",
            "head -n 5",
            "head -n5",
            "head --lines=5",
            "'head' -20",
        ] {
            same(&format!("rg a | {head}"), &format!("{g} a | {head}"));
            same(
                &format!("rg a |{head}; rg b"),
                &format!("{g} a |{head}; {g} b"),
            );
        }
        let r = rewrite_full("grep -rn a src | head -20").unwrap();
        assert!(r.command.starts_with("greeg ") && r.command.ends_with(" a src | head -20"));
        assert!(r.whole && r.parts[0].piped);
        assert!(!rewrite_full("rg a").unwrap().parts[0].piped);
        for (cmd, why) in [
            ("rg a | head -c 5", "pipeline"),
            ("rg a | head -n -5", "pipeline"),
            ("rg a | head -5 out.txt", "pipeline"),
            ("rg a | head -5K", "pipeline"),
            ("rg a | head -5 | cat", "pipeline"),
            ("rg a | grep b | head", "pipeline"),
            ("cat f | rg a", "pipeline"),
            ("echo a | rg a | head", "pipeline"),
            ("rg a |& head", "pipeline of stderr"),
            ("rg a |\n head", "pipeline across lines"),
            ("rg a | \n rg b", "pipeline across lines"),
            ("rg a | head; export X=1", "change the environment"),
            ("rg a; X=1 cmd | head", "change the environment"),
        ] {
            let got = rewrite_full(cmd).err().unwrap_or_default();
            assert!(got.contains(why), "{cmd:?}: {got:?}");
        }
        // a pipeline into another reader is left as it is
        let r = rewrite_full("rg a | sort; rg b").unwrap();
        assert!(!r.whole && r.parts.len() == 1);
    }

    #[test]
    fn redirections_that_leave_the_output_alone_are_kept() {
        let g = "greeg --matching exact --max-filesize 18446744073709551615";
        for kept in [
            "2>/dev/null",
            "2> /dev/null",
            "2>>/dev/null",
            "2>> /dev/null",
            "2>&1",
            "</dev/null",
            "< /dev/null",
            "2>&1 </dev/null",
            "2>&1  ",
        ] {
            same(&format!("rg a {kept}"), &format!("{g} a {kept}"));
        }
        // stdin's `<` needs no blank before it
        same("rg a</dev/null", &format!("{g} a</dev/null"));
        same("rg a 2>&1</dev/null", &format!("{g} a 2>&1</dev/null"));
        same("rg a\\</dev/null", &format!("{g} 'a</dev/null'"));
        same("rg a 2>&1 | head -3", &format!("{g} a 2>&1 | head -3"));
        same("rg a\\  2>&1", &format!("{g} 'a ' 2>&1"));
        same(
            "rg a 2>/dev/null; rg b\t2>&1",
            &format!("{g} a 2>/dev/null; {g} b\t2>&1"),
        );
        let r = rewrite_full("grep -rn a . 2>&1 | head").unwrap();
        assert!(r.command.ends_with(" a . 2>&1 | head"), "{}", r.command);
        for (cmd, why) in [
            ("rg a >/dev/null", "redirection"),
            ("rg a > out.txt", "redirection"),
            ("rg a 2>err.txt", "redirection"),
            ("rg a 2>&1 >/dev/null", "redirection"),
            ("rg a &>/dev/null", "background job"),
            ("rg 2>/dev/null a", "redirection"),
            ("rg a\\ 2>&1", "redirection"),
            ("rg a 12>&1", "redirection"),
            ("rg a2>&1", "redirection"),
            ("rg 'a'2>/dev/null", "redirection"),
            ("2>&1", "redirection"),
            ("grep -rn a . 2>/dev/null", "hide that greeg skips"),
            ("grep -rl a . 2> /dev/null | head", "hide that greeg skips"),
            ("grep -rl a . 2>> /dev/null", "hide that greeg skips"),
        ] {
            let got = rewrite_full(cmd).err().unwrap_or_default();
            assert!(got.contains(why), "{cmd:?}: {got:?}");
        }
    }

    #[test]
    fn only_a_command_the_line_starts_with_is_replayed() {
        let r = rewrite_full("rg a; cd src && rg b; rg c").unwrap();
        let after: Vec<_> = r.parts.iter().map(|p| p.after_other).collect();
        assert_eq!(after, [false, true, true]);
        let r = rewrite_full("rg a && rg b").unwrap();
        assert!(r.parts.iter().all(|p| !p.after_other));
    }

    #[test]
    fn an_unquoted_include_glob_is_taken_only_where_the_shell_keeps_it() {
        let dir = std::env::temp_dir().join(format!("greeg-glob-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cmd = "grep -rn needle --include=*.rs .";
        let rw = decide(cmd, Agent::Claude, &dir).unwrap();
        assert!(rw.command.contains("-g '*.rs'"), "{}", rw.command);
        // bash would expand the glob to this name
        std::fs::write(dir.join("--include=x.rs"), "").unwrap();
        let why = decide(cmd, Agent::Claude, &dir).err().unwrap_or_default();
        assert!(why.contains("would expand"), "{why}");
        assert!(decide("grep -rn needle '--include=*.rs' .", Agent::Claude, &dir).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
        // after a `cd` the directory is not known
        let why = rewrite_full("cd src && grep -rn a --include=*.rs .")
            .err()
            .unwrap_or_default();
        assert!(why.contains("directory not known"), "{why}");
        assert!(rewrite_full("cd src && grep -rn a --include='*.rs' .").is_ok());
        assert!(rewrite_full("rg x; grep -rn a --include=*.rs .").is_ok());
    }

    #[test]
    fn codex_gets_a_list_only_when_the_hook_wrote_every_command_in_it() {
        // Codex runs an allowed rewrite without asking
        for cmd in ["rg a; rm -rf build", "cd src && rg a", "rg a || curl x"] {
            assert!(decide(cmd, Agent::Claude, Path::new(".")).is_ok(), "{cmd}");
            let why = decide(cmd, Agent::Codex, Path::new("."))
                .err()
                .unwrap_or_default();
            assert!(why.contains("Codex approves"), "{cmd}: {why}");
        }
        // `head -N` reads only its stdin: a line of rewrites and such heads is whole
        for cmd in ["rg a | sort", "cd src && rg a | head", "rg a | head; rm x"] {
            assert!(decide(cmd, Agent::Codex, Path::new(".")).is_err(), "{cmd}");
        }
        for cmd in [
            "rg a",
            "rg a; rg -l b src",
            "rg a && rg b",
            "rg a | head -5; rg b",
        ] {
            let rw = decide(cmd, Agent::Codex, Path::new(".")).unwrap();
            assert!(rw.whole && !rw.command.contains("rg "), "{cmd}");
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
            rewrite_full(cmd).ok().map(|r| r.command).as_deref(),
            Some(want),
            "{cmd}"
        );
    }

    #[track_caller]
    fn untouched(cmd: &str) {
        assert!(rewrite_full(cmd).is_err(), "unexpected rewrite: {cmd:?}");
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
            "rg foo 2>err.txt",
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
            assert_eq!(r.parts[0].original, ["rg", pattern, "src"]);
            assert_eq!(
                r.parts[0].rewritten,
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
            assert_eq!(shell_words(&r.command).unwrap(), r.parts[0].rewritten);
        }
        same(
            "rg -g '*.rs' foo",
            "greeg --matching exact --max-filesize 18446744073709551615 -g '*.rs' foo",
        );
    }

    #[test]
    fn hook_reply_shapes() {
        let claude = hook_reply(Agent::Claude, &json!({"command": "rg foo"}), "greeg foo");
        assert_eq!(
            claude,
            json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecisionReason": "greeg rewrite", "updatedInput": {"command": "greeg foo"}}})
        );
        let codex = hook_reply(Agent::Codex, &json!({"command": "rg foo"}), "greeg foo");
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
