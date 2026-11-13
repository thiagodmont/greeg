//! Hook rewrites through real shells and greeg's own CLI parser. Argument
//! shims named `rg`, `grep` and `greeg` record what each shell hands them;
//! the rewritten greeg words must parse as a search of the pattern and paths
//! the original command searched, and no other search program may run.
//! (A unit test, not one under tests/: the parser is the binary's `Cli`.)
use crate::Cli;
use clap::Parser;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Shims {
    dir: PathBuf,
}

impl Shims {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "greeg-shell-rewrite-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        for name in ["rg", "grep", "greeg"] {
            let p = dir.join("bin").join(name);
            // one record per run: the program, then each argument NUL-terminated
            std::fs::write(
                &p,
                format!("#!/bin/sh\n{{ printf '%s\\0' {name} \"$@\"; printf '\\036'; }} >> \"$SHIM_LOG\"\n"),
            )
            .unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { dir }
    }

    /// The argv of every shimmed program `line` ran, in order.
    fn run(&self, shell: &[&str], line: &str) -> Vec<Vec<String>> {
        let log = self.dir.join("log");
        let _ = std::fs::remove_file(&log);
        let out = Command::new(shell[0])
            .args(&shell[1..])
            .arg(line)
            .current_dir(&self.dir)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.dir.join("bin").display()),
            )
            .env("SHIM_LOG", &log)
            .output()
            .unwrap();
        assert!(out.status.success(), "{shell:?} {line}: {out:?}");
        let bytes = std::fs::read(&log).unwrap_or_default();
        String::from_utf8(bytes)
            .unwrap()
            .split('\u{1e}')
            .filter(|r| !r.is_empty())
            .map(|r| {
                r.strip_suffix('\0')
                    .unwrap()
                    .split('\0')
                    .map(str::to_owned)
                    .collect()
            })
            .collect()
    }
}

impl Drop for Shims {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn shells() -> Vec<&'static [&'static str]> {
    [&["/bin/bash", "-c"][..], &["/bin/zsh", "-f", "-c"]]
        .into_iter()
        .filter(|s| Path::new(s[0]).exists())
        .collect()
}

/// A search's pattern and paths.
type Search = (&'static str, &'static [&'static str]);

#[test]
fn rewrites_reach_greeg_as_the_search_the_original_ran() {
    let shims = Shims::new();
    let shells = shells();
    assert!(!shells.is_empty(), "neither bash nor zsh");
    // (line, the (pattern, paths) of each search in it)
    let table: &[(&str, &[Search])] = &[
        ("rg foo src", &[("foo", &["src"])]),
        // operands named like subcommands (1.1)
        ("rg foo index", &[("foo", &["index"])]),
        ("rg index index", &[("index", &["index"])]),
        ("rg -l foo budget none", &[("foo", &["budget", "none"])]),
        ("grep -rn foo map", &[("foo", &["map"])]),
        // words starting with `=`, which zsh expands (1.3)
        ("rg '==' src", &[("==", &["src"])]),
        ("rg '=ls' src", &[("=ls", &["src"])]),
        ("grep -rn '=x' src", &[("=x", &["src"])]),
        // an attached short-flag value loses one `=` (1.6)
        ("rg -e=foo src", &[("foo", &["src"])]),
        ("rg -g='*.rs' foo src", &[("foo", &["src"])]),
        ("rg -t=py foo", &[("foo", &[])]),
        // `--` forms and patterns that look like flags
        ("rg -- -x src", &[("-x", &["src"])]),
        ("rg -e -dash src", &[("-dash", &["src"])]),
        ("rg foo -- -weird", &[("foo", &["-weird"])]),
        // quoting
        ("rg 'a b' 'my dir'", &[("a b", &["my dir"])]),
        (
            "rg '$(touch sentinel)' src",
            &[("$(touch sentinel)", &["src"])],
        ),
        ("rg 'a'\\''b' src", &[("a'b", &["src"])]),
        ("rg \\# src", &[("#", &["src"])]),
        ("rg '' src", &[("", &["src"])]),
        // lists and head
        (
            "rg foo src; rg bar lib",
            &[("foo", &["src"]), ("bar", &["lib"])],
        ),
        (
            "rg foo src && rg -w bar",
            &[("foo", &["src"]), ("bar", &[])],
        ),
        ("rg foo src | head -5", &[("foo", &["src"])]),
        ("grep -rn foo src | sort", &[("foo", &["src"])]),
    ];
    for (line, searches) in table {
        let rewritten = crate::rewrite::rewrite_full(line)
            .unwrap_or_else(|e| panic!("{line}: {e}"))
            .command;
        for shell in &shells {
            let original = shims.run(shell, line);
            assert_eq!(
                original.len(),
                searches.len(),
                "{shell:?} {line}: {original:?}"
            );
            assert!(
                original.iter().all(|a| a[0] == "rg" || a[0] == "grep"),
                "{line}"
            );
            let ran = shims.run(shell, &rewritten);
            assert_eq!(ran.len(), searches.len(), "{shell:?} {rewritten}: {ran:?}");
            for (argv, (pattern, paths)) in ran.iter().zip(searches.iter()) {
                assert_eq!(argv[0], "greeg", "{shell:?} {rewritten}: only greeg runs");
                let cli = Cli::try_parse_from(argv)
                    .unwrap_or_else(|e| panic!("{shell:?} {rewritten}: {argv:?}: {e}"));
                assert!(cli.cmd.is_none(), "{shell:?} {rewritten}: {argv:?}");
                let got = cli
                    .regexp
                    .first()
                    .or(cli.pattern.as_ref())
                    .map(String::as_str);
                assert_eq!(got, Some(*pattern), "{shell:?} {rewritten}: {argv:?}");
                let got: Vec<String> = cli.paths.iter().map(|p| p.display().to_string()).collect();
                assert_eq!(got, *paths, "{shell:?} {rewritten}: {argv:?}");
            }
        }
    }
    assert!(!shims.dir.join("sentinel").exists());
}
