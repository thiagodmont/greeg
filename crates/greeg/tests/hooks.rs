//! Hook protocol and shell argument tests on disposable local fixtures.
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_greeg");
static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn allocate_root(parent: &Path, mut id: usize) -> PathBuf {
        loop {
            let root = parent.join(format!("greeg-hooks-{}-{id}", std::process::id()));
            match fs::create_dir(&root) {
                Ok(()) => return root,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => id += 1,
                Err(e) => panic!("create fixture: {e}"),
            }
        }
    }

    fn new() -> Self {
        let root = Self::allocate_root(&std::env::temp_dir(), NEXT.fetch_add(1, Ordering::Relaxed));
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("tree/.git")).unwrap();
        fs::write(root.join("tree/file.rs"), "pub fn needle() {}\n").unwrap();
        Self(root)
    }

    fn command(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        c.current_dir(self.0.join("tree"))
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .env("GREEG_INDEX_DIR", self.0.join("index"))
            .env("GREEG_STATS_DIR", self.0.join("stats"))
            .env("GREEG_STATS", "0")
            .env_remove("GREEG_SESSION")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("RIPGREP_CONFIG_PATH");
        c
    }

    fn hook(&self, agent: &str, command: &str, config: bool) -> Output {
        let mut c = self.command(BIN);
        c.args(["hook", "run", "--agent", agent])
            .env("GREEG_STATS", "1");
        if config {
            c.env("RIPGREP_CONFIG_PATH", self.0.join("unread-config"));
        }
        let mut child = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let payload = json!({"tool_name": "Bash", "tool_input": {"command": command}, "cwd": self.0.join("tree")});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn rewrite(&self, agent: &str, command: &str) -> String {
        let out = self.hook(agent, command, false);
        assert!(out.status.success(), "{out:?}");
        assert!(out.stderr.is_empty());
        let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
        let specific = &reply["hookSpecificOutput"];
        assert_eq!(specific["hookEventName"], "PreToolUse");
        if agent == "codex" {
            assert_eq!(specific["permissionDecision"], "allow");
        } else {
            assert!(specific.get("permissionDecision").is_none());
        }
        specific["updatedInput"]["command"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn search(&self, rewritten: &str, indexed: bool) -> Output {
        let body = rewritten.strip_prefix("greeg ").unwrap();
        let mode = if indexed {
            "--fresh stat"
        } else {
            "--no-index"
        };
        self.command("/bin/sh")
            .args([
                "-c",
                &format!("exec \"$1\" --no-session {mode} {body}"),
                "hook-test",
                BIN,
            ])
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn fixture_allocation_preserves_existing_directories() {
    let parent = Fixture::new();
    let stale = Fixture::allocate_root(&parent.0, 0);
    fs::write(stale.join("sentinel"), b"keep").unwrap();
    let fresh = Fixture::allocate_root(&parent.0, 0);
    assert_ne!(stale, fresh);
    assert_eq!(fs::read(stale.join("sentinel")).unwrap(), b"keep");
    assert_eq!(fs::read_dir(fresh).unwrap().count(), 0);
}

#[test]
fn declined_commands_emit_no_reply_or_stats() {
    let f = Fixture::new();
    for agent in ["claude", "codex"] {
        for command in [
            "grep -rl needle .",
            "grep needle file.rs",
            "rg --json needle",
            "./rg needle",
            "rg needle # comment",
            "rg needle\\\n file.rs",
            "rg -g*.rs needle",
            "rg needle | head",
            "rg needle && touch sentinel",
            "rg needle; touch sentinel",
            "cd . && rg needle",
            "rg --pre cat needle",
            "rg --ignore-case=false needle",
        ] {
            let out = f.hook(agent, command, false);
            assert!(
                out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(),
                "{command}: {out:?}"
            );
        }
        let out = f.hook(agent, "rg needle", true);
        assert!(out.status.success() && out.stdout.is_empty() && out.stderr.is_empty());
    }
    assert!(!f.0.join("stats").exists());
    assert!(!f.0.join("tree/sentinel").exists());
}

#[test]
fn rewritten_queries_keep_exact_hits_and_misses_on_both_backends() {
    let f = Fixture::new();
    let build = f.command(BIN).args(["index", "--quiet"]).output().unwrap();
    assert!(build.status.success(), "{build:?}");
    for agent in ["claude", "codex"] {
        for indexed in [false, true] {
            for (command, expected, status) in [
                ("rg -l needle", "file.rs\n", 0),
                ("rg -c needle", "file.rs:1\n", 0),
                ("rg -l NEEDLE", "", 1),
                ("rg -w -c needl", "", 1),
                ("rg -i -l NEEDLE", "file.rs\n", 0),
                ("rg -l -F 'needle()'", "file.rs\n", 0),
            ] {
                let rewritten = f.rewrite(agent, command);
                let out = f.search(&rewritten, indexed);
                assert_eq!(out.status.code(), Some(status), "{command}: {out:?}");
                assert_eq!(out.stdout, expected.as_bytes(), "{command}");
                assert!(!String::from_utf8_lossy(&out.stderr).contains("matched "));
            }
            let rewritten = f.rewrite(agent, "rg NEEDLE");
            let out = f.search(&rewritten, indexed);
            assert_eq!(out.status.code(), Some(1));
            assert!(!String::from_utf8_lossy(&out.stdout).contains("file.rs"));
        }
    }
    let events = fs::read_to_string(f.0.join("stats/events.jsonl")).unwrap();
    assert_eq!(events.lines().count(), 28);
    for event in events.lines() {
        let v: Value = serde_json::from_str(event).unwrap();
        assert_eq!(v["original"][0], "rg");
        assert_eq!(v["rewritten"][1], "--matching");
        assert_eq!(v["rewritten"][2], "exact");
    }
}

#[test]
fn rewritten_searches_preserve_file_size_selection() {
    let f = Fixture::new();
    let mut body = vec![b'x'; (4 << 20) + 1];
    body.extend_from_slice(b"\nlarge_needle\n");
    fs::write(f.0.join("tree/large.txt"), body).unwrap();
    let build = f.command(BIN).args(["index", "--quiet"]).output().unwrap();
    assert!(build.status.success(), "{build:?}");
    for agent in ["claude", "codex"] {
        for indexed in [false, true] {
            for (command, expected, code) in [
                ("rg -l large_needle", "large.txt\n", 0),
                ("rg -l --max-filesize 4194304 large_needle", "", 1),
                (
                    "rg -l --max-filesize=5242880 large_needle",
                    "large.txt\n",
                    0,
                ),
                ("rg -l --max-filesize 0 needle", "", 1),
            ] {
                let out = f.search(&f.rewrite(agent, command), indexed);
                assert_eq!(out.status.code(), Some(code), "{command}: {out:?}");
                assert_eq!(out.stdout, expected.as_bytes(), "{command}");
            }
        }
    }
}

#[test]
fn shell_receives_literal_arguments_without_side_effects() {
    let f = Fixture::new();
    for (command, pattern) in [
        ("rg '$(touch sentinel)' 'a b'", "$(touch sentinel)"),
        ("rg '; touch sentinel' 'a b'", "; touch sentinel"),
        ("rg '\"quoted\"' 'a b'", "\"quoted\""),
        ("rg 'a'\\''b' 'a b'", "a'b"),
        ("rg '' 'a b'", ""),
        ("rg \\# 'a b'", "#"),
    ] {
        let rewritten = f.rewrite("codex", command);
        let args = rewritten.strip_prefix("greeg ").unwrap();
        let out = f
            .command("/bin/sh")
            .args(["-c", &format!("set -- {args}; printf '%s\\000' \"$@\"")])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(
            out.stdout,
            format!("--matching\0exact\0--max-filesize\018446744073709551615\0{pattern}\0a b\0")
                .as_bytes()
        );
    }
    assert!(!f.0.join("tree/sentinel").exists());
}
