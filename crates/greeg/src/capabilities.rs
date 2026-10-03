//! `greeg --capabilities`: what this build supports, one JSON record for
//! adapters. Commands and flags come from the parser itself.

use crate::{Cli, budget, hook::Agent, json_native, stats};
use clap::{CommandFactory, ValueEnum};
use serde_json::{Value, json};
use std::io::Write;

const SCHEMA: u32 = 1;

pub fn run() -> anyhow::Result<()> {
    let mut out = serde_json::to_vec(&record())?;
    out.push(b'\n');
    std::io::stdout().write_all(&out)?;
    Ok(())
}

fn record() -> Value {
    let mut cli = Cli::command();
    // building adds the flags the parser makes itself (`--version`)
    cli.build();
    let (budget, source) = budget::in_effect();
    let extra: Vec<Value> = greeg_lang::extra::registry()
        .iter()
        .enumerate()
        .map(|(i, l)| {
            json!({
                "name": l.name,
                "extensions": l.extensions,
                "ready": greeg_lang::sym::extra_ready(i as u8),
            })
        })
        .collect();
    let global: Vec<String> = cli
        .get_arguments()
        .filter(|a| a.is_global_set() && !a.is_hide_set())
        .filter_map(|a| a.get_long())
        .map(|l| format!("--{l}"))
        .collect();
    json!({
        "type": "capabilities",
        "data": {
            "schema": SCHEMA,
            "version": stats::VERSION,
            "index_format": greeg_index::FORMAT_VERSION,
            "json": {
                "dialects": ["greeg", "legacy", "rg"],
                "greeg_schema": json_native::SCHEMA,
                "bare": "legacy",
            },
            "global": global,
            "search": arguments(&cli, &global),
            "commands": subcommands(&cli, &global),
            "budget": {
                "levels": budget::LEVELS.iter().map(|(n, v)| (n.to_string(), json!(v))).collect::<serde_json::Map<_, _>>(),
                "default": budget,
                "source": source,
                "max_bytes": true,
            },
            "matching": ["exact", "discover"],
            "fresh": {
                "modes": ["auto", "none", "stat", "fsevents"],
                "fsevents": cfg!(target_os = "macos"),
            },
            "languages": {
                "builtin": greeg_lang::BUILTINS
                    .iter()
                    .filter(|l| l.has_grammar())
                    .map(|l| l.name())
                    .collect::<Vec<_>>(),
                "extra": extra,
            },
            "agents": Agent::value_variants()
                .iter()
                .filter_map(|a| a.to_possible_value())
                .map(|v| v.get_name().to_string())
                .collect::<Vec<_>>(),
        }
    })
}

/// A command's arguments: positional names and the visible long flags it
/// declares, leaving out those listed above it (`above`: global ones and its
/// parents' flags, which clap propagates).
fn arguments(c: &clap::Command, above: &[String]) -> Value {
    let args: Vec<String> = c
        .get_positionals()
        .filter(|a| !a.is_hide_set())
        .map(|a| a.get_id().to_string())
        .collect();
    let flags: Vec<String> = c
        .get_arguments()
        .filter(|a| !a.is_hide_set())
        .filter_map(|a| a.get_long())
        .map(|l| format!("--{l}"))
        .filter(|f| !above.contains(f))
        .collect();
    json!({ "args": args, "flags": flags })
}

/// Each subcommand's arguments, and its own subcommands under `commands`.
fn subcommands(c: &clap::Command, above: &[String]) -> Value {
    c.get_subcommands()
        .filter(|s| s.get_name() != "help" && !s.is_hide_set())
        .map(|s| {
            let mut v = arguments(s, above);
            if s.has_subcommands() {
                let mut listed = above.to_vec();
                listed.extend(
                    v["flags"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|f| f.as_str().map(String::from)),
                );
                v["commands"] = subcommands(s, &listed);
            }
            (s.get_name().to_string(), v)
        })
        .collect::<serde_json::Map<_, _>>()
        .into()
}
