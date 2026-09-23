//! `greeg stats replay` runs only a trusted ripgrep, on disposable fixtures.
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_greeg");
static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

impl Fixture {
    /// A tree with one match, a planted `./rg` that would leave a sentinel,
    /// and an absolute fake ripgrep that logs how it was called.
    fn new() -> Self {
        let root = loop {
            let root = std::env::temp_dir().join(format!(
                "greeg-replay-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::DirBuilder::new().mode(0o700).create(&root) {
                Ok(()) => break root,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create fixture: {e}"),
            }
        };
        for d in ["home", "tree/.git", "bin"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("tree/file.rs"), "pub fn needle() {}\n").unwrap();
        script(
            &root.join("tree/rg"),
            &format!("touch {}", root.join("sentinel").display()),
        );
        script(
            &root.join("bin/rg"),
            &format!(
                "if [ \"$1\" = --version ]; then echo 'ripgrep 0.0.0-fake'; exit 0; fi\n\
                 echo \"args=$* config=$RIPGREP_CONFIG_PATH\" >> {}\n\
                 echo file.rs:1:needle",
                root.join("rg.log").display()
            ),
        );
        Self(root)
    }

    fn command(&self) -> Command {
        let mut c = Command::new(BIN);
        c.current_dir(self.0.join("tree"))
            .env("HOME", self.0.join("home"))
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .env("GREEG_INDEX_DIR", self.0.join("index"))
            .env("GREEG_STATS_DIR", self.0.join("stats"))
            .env("GREEG_STATS", "1")
            .env_remove("GREEG_SESSION")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("RIPGREP_CONFIG_PATH");
        c
    }

    /// Record a hook rewrite of `rg needle` and the greeg run it produced.
    fn record(&self) {
        let mut hook = self
            .command()
            .args(["hook", "run"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let payload = json!({"tool_name": "Bash", "tool_input": {"command": "rg needle"}, "cwd": self.0.join("tree")});
        hook.stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        assert!(hook.wait_with_output().unwrap().status.success());
        let rewritten = self.last_hook()["rewritten"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .map(|w| w.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let run = self.command().args(&rewritten).output().unwrap();
        assert_eq!(run.status.code(), Some(0), "{run:?}");
    }

    fn last_hook(&self) -> Value {
        fs::read_to_string(self.0.join("stats/events.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .rfind(|v| v["kind"] == "hook")
            .unwrap()
    }

    /// Append a copy of the last hook record with a different original argv.
    fn tamper(&self, original: &[&str]) {
        let events = self.0.join("stats/events.jsonl");
        let mut hook = self.last_hook();
        hook["original"] = json!(original);
        hook["ts"] = json!(hook["ts"].as_u64().unwrap() + 1);
        let mut f = fs::OpenOptions::new().append(true).open(&events).unwrap();
        writeln!(f, "{hook}").unwrap();
    }

    fn replay(&self, extra: &[&str], path_var: Option<&str>) -> Output {
        let mut c = self.command();
        c.args(["stats", "replay", "--runs", "1", "--force"])
            .args(extra)
            .env("GREEG_STATS", "0")
            .env("RIPGREP_CONFIG_PATH", self.0.join("unread-config"));
        if let Some(p) = path_var {
            c.env("PATH", p);
        }
        c.output().unwrap()
    }

    fn rg_log(&self) -> String {
        fs::read_to_string(self.0.join("rg.log")).unwrap_or_default()
    }
}

#[test]
fn replay_runs_the_trusted_ripgrep_without_inherited_configuration() {
    let f = Fixture::new();
    f.record();
    let rg = f.0.join("bin/rg");
    let out = f.replay(&["--rg", rg.to_str().unwrap()], None);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(f.rg_log(), "args=needle config=\n".repeat(2));
    let replay = fs::read_to_string(f.0.join("stats/replay.jsonl")).unwrap();
    assert!(
        replay.contains("\"rg_version\":\"ripgrep 0.0.0-fake\""),
        "{replay}"
    );
    assert!(!f.0.join("sentinel").exists());
}

#[test]
fn replay_never_runs_a_recorded_path_or_a_relative_path_entry() {
    let f = Fixture::new();
    f.record();
    f.tamper(&["./rg", "needle"]);
    let rg = f.0.join("bin/rg");
    let out = f.replay(&["--rg", rg.to_str().unwrap()], None);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && stderr.contains("not rg"),
        "{stderr}"
    );

    // `rg` resolved through `.` would be the planted script
    f.tamper(&["rg", "needle"]);
    let out = f.replay(&[], Some(".:bin"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && stderr.contains("absolute PATH entry"),
        "{stderr}"
    );
    let out = f.replay(&["--rg", "rg"], None);
    assert!(!out.status.success(), "{out:?}");
    assert!(!f.0.join("sentinel").exists());
    assert_eq!(f.rg_log(), "");
}

#[test]
fn nothing_to_replay_needs_no_ripgrep() {
    let f = Fixture::new();
    let out = f.replay(&[], Some(""));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stderr.contains("nothing to replay"),
        "{stderr}"
    );
}
