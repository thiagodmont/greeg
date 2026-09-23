//! Session privacy and search behavior in isolated homes and index directories.
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = loop {
            let candidate = std::env::temp_dir().join(format!(
                "greeg-session-cli-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("fixture directory: {e}"),
            }
        };
        fs::create_dir(path.join("home")).unwrap();
        fs::write(path.join("file.rs"), "pub fn needle() {}\n").unwrap();
        Self(path)
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_greeg"));
        c.current_dir(&self.0)
            .args(["needle", "file.rs", "--no-index", "--budget", "0"])
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .env("GREEG_INDEX_DIR", self.0.join("index"))
            .env("GREEG_STATS_DIR", self.0.join("stats"))
            .env("GREEG_STATS", "0")
            .env_remove("GREEG_SESSION")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .stdin(Stdio::null());
        c
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn permissive_umask_still_creates_private_memory_and_preserves_search_output() {
    let f = Fixture::new();
    fs::create_dir(f.0.join("index")).unwrap();
    fs::set_permissions(f.0.join("index"), fs::Permissions::from_mode(0o755)).unwrap();
    let plain = f.command().arg("--no-session").output().unwrap();
    let mut c = f.command();
    c.args(["--session", "private"]);
    // SAFETY: umask is async-signal-safe and runs only in this child.
    unsafe {
        c.pre_exec(|| {
            libc::umask(0);
            Ok(())
        });
    }
    let run = c.output().unwrap();
    assert!(run.status.success(), "{run:?}");
    assert_eq!(run.stdout, plain.stdout);
    assert_eq!(run.stderr, plain.stderr);
    for (name, mode) in [
        ("index", 0o755),
        ("index/session", 0o700),
        ("index/session/private.jsonl", 0o600),
        ("index/session/.lock", 0o600),
    ] {
        assert_eq!(
            fs::metadata(f.0.join(name)).unwrap().mode() & 0o7777,
            mode,
            "{name}"
        );
    }
    let record: serde_json::Value = serde_json::from_str(
        fs::read_to_string(f.0.join("index/session/private.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert!(record.get("pat").is_none());
}

#[test]
fn no_session_has_no_session_side_effects_and_unsafe_storage_does_not_fail_search() {
    let f = Fixture::new();
    let plain = f.command().arg("--no-session").output().unwrap();
    assert!(plain.status.success());
    assert!(!f.0.join("index").exists());
    fs::create_dir(f.0.join("index")).unwrap();
    let outside = f.0.join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, f.0.join("index/session")).unwrap();
    let run = f.command().args(["--session", "unsafe"]).output().unwrap();
    assert!(run.status.success());
    assert_eq!(run.stdout, plain.stdout);
    assert_eq!(run.stderr, plain.stderr);
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
}

#[test]
fn simultaneous_processes_leave_complete_records() {
    let f = Fixture::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut recorded = 0;
    while recorded < 8 {
        let children: Vec<_> = (recorded..8)
            .map(|_| {
                f.command()
                    .args(["--session", "parallel"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in children {
            let run = child.wait_with_output().unwrap();
            assert!(run.status.success(), "{run:?}");
        }
        let text = match fs::read_to_string(f.0.join("index/session/parallel.jsonl")) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => panic!("session log: {e}"),
        };
        for line in text.lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(row["q"], "needle");
        }
        let count = text.lines().count();
        assert!((recorded..=8).contains(&count));
        recorded = count;
        // Best-effort writes may skip a record after the bounded lock wait.
        assert!(
            recorded == 8 || std::time::Instant::now() < deadline,
            "only {recorded}/8 complete records after retries"
        );
    }
}

#[test]
fn empty_environment_session_keeps_parent_process_isolation() {
    let f = Fixture::new();
    let run = f.command().env("GREEG_SESSION", "").output().unwrap();
    assert!(run.status.success());
    assert!(
        f.0.join(format!("index/session/p{}.jsonl", std::process::id()))
            .is_file()
    );
}
