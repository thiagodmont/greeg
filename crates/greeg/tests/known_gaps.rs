//! Reproduced defects that are not fixed yet. Each test asserts the intended
//! behavior and stays ignored until the fix lands; CI fails when an ignored
//! test here starts passing, so a fix must remove its `#[ignore]`.
//!
//! List them with `cargo test --test known_gaps -- --ignored`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_greeg");
static N: AtomicU32 = AtomicU32::new(0);

/// Longer than the freshness reuse window, so the next query checks the tree.
const PAST_FRESHNESS_WINDOW: Duration = Duration::from_millis(300);

struct Fixture {
    base: PathBuf,
    root: PathBuf,
    index: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn w(p: &Path, body: impl AsRef<[u8]>) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

impl Fixture {
    /// A directory this fixture creates itself, so `Drop` never removes one
    /// left behind by another run.
    fn allocate_base() -> PathBuf {
        loop {
            let base = std::env::temp_dir().join(format!(
                "greeg-gaps-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&base) {
                Ok(()) => return base,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create fixture: {e}"),
            }
        }
    }

    fn new(files: &[(&str, &str)]) -> Self {
        let base = Self::allocate_base();
        let root = base.join("tree");
        let index = base.join("index");
        fs::create_dir_all(root.join(".git/info")).unwrap();
        for (name, body) in files {
            w(&root.join(name), body);
        }
        Fixture { base, root, index }
    }

    /// Enough untouched files that one edit stays below the rebuild
    /// threshold and exercises the incremental path.
    fn with_filler(files: &[(&str, &str)]) -> Self {
        let f = Fixture::new(files);
        for i in 0..100 {
            w(
                &f.root.join(format!("filler/f{i:03}.txt")),
                format!("filler {i}\n"),
            );
        }
        f
    }

    fn command(&self) -> Command {
        let mut c = Command::new(BIN);
        c.current_dir(&self.root)
            .env("HOME", self.base.join("home"))
            .env("XDG_CACHE_HOME", self.base.join("cache"))
            .env("GREEG_STATS", "0")
            .env("GREEG_INDEX_DIR", &self.index)
            .env_remove("GREEG_SESSION")
            .env_remove("RIPGREP_CONFIG_PATH");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .args(["--no-session", "--index-dir"])
            .arg(&self.index)
            .output()
            .expect("run greeg")
    }

    fn scan(&self, args: &[&str]) -> Output {
        let mut a = args.to_vec();
        a.push("--no-index");
        self.run(&a)
    }

    fn indexed(&self) -> &Self {
        let o = self
            .command()
            .args(["index", "--quiet", "--index-dir"])
            .arg(&self.index)
            .output()
            .expect("build index");
        assert!(o.status.success(), "index: {o:?}");
        std::thread::sleep(PAST_FRESHNESS_WINDOW);
        self
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Paths from a `def` answer, in order.
fn def_paths(o: &Output) -> Vec<String> {
    stdout(o)
        .lines()
        .skip(1)
        .take_while(|l| !l.starts_with("next:"))
        .filter(|l| !l.starts_with(' '))
        .map(|l| l.split("  ").next().unwrap().to_string())
        .collect()
}

#[test]
#[ignore = "known gap: indexed symbol verbs ignore request filters"]
fn indexed_definitions_honor_request_filters() {
    let f = Fixture::new(&[
        ("src/lib.rs", "pub fn needle() {}\n"),
        ("src/lib.py", "def needle():\n    pass\n"),
        ("tests/test.rs", "pub fn needle() {}\n"),
    ]);
    f.indexed();
    for (flags, want) in [
        (vec!["--no-tests"], vec!["src/lib.py", "src/lib.rs"]),
        (vec!["-t", "py"], vec!["src/lib.py"]),
        (vec!["-g", "src/lib.py"], vec!["src/lib.py"]),
    ] {
        let mut args = vec!["def", "needle", "--budget", "0"];
        args.extend(&flags);
        let mut indexed = def_paths(&f.run(&args));
        let mut scanned = def_paths(&f.scan(&args));
        indexed.sort();
        scanned.sort();
        assert_eq!(scanned, want, "scan {flags:?}");
        assert_eq!(indexed, want, "index {flags:?}");
    }
}

#[test]
#[ignore = "known gap: kind filtering runs after the retained-hit cap"]
fn kind_filters_apply_before_the_retained_hit_cap() {
    let late = format!("{}fn late() {{ marker(); }}\n", "// marker\n".repeat(70));
    let f = Fixture::new(&[("src/late.rs", &late)]);
    f.indexed();
    let args = ["marker", "--kind", "call", "-l"];
    for o in [f.run(&args), f.scan(&args)] {
        assert_eq!(
            (o.status.code(), stdout(&o)),
            (Some(0), "src/late.rs\n".into())
        );
    }
}

#[test]
#[ignore = "known gap: same-line mixed kinds are classified by the first occurrence"]
fn a_call_after_a_same_line_comment_is_found() {
    let f = Fixture::new(&[("b.rs", "fn f() { /* target */ target(); }\n")]);
    f.indexed();
    let args = ["target", "--kind", "call", "--budget", "0"];
    for o in [f.run(&args), f.scan(&args)] {
        assert_eq!(o.status.code(), Some(0), "{o:?}");
        assert!(stdout(&o).contains("b.rs:1:"), "{o:?}");
    }
}

#[test]
#[ignore = "known gap: scan classification has no multiline comment state"]
fn multiline_comment_text_is_not_a_call_in_scan_mode() {
    let f = Fixture::new(&[("a.rs", "/*\nneedle();\n*/\nfn f() {}\n")]);
    let call = f.scan(&["needle", "--kind", "call", "--budget", "0"]);
    assert_eq!(
        (call.status.code(), stdout(&call)),
        (Some(1), String::new())
    );
    let comment = f.scan(&["needle", "--kind", "comment", "--budget", "0"]);
    assert!(stdout(&comment).contains("a.rs:2:"), "{comment:?}");
}

#[test]
#[ignore = "known gap: scan mode keeps header-marked generated files"]
fn scan_excludes_header_marked_generated_files() {
    let f = Fixture::new(&[(
        "src/auto.rs",
        "// Code generated by generator. DO NOT EDIT.\npub fn generatedneedle() {}\n",
    )]);
    f.indexed();
    let args = ["generatedneedle", "--no-generated", "--budget", "0"];
    for o in [f.run(&args), f.scan(&args)] {
        assert_eq!((o.status.code(), stdout(&o)), (Some(1), String::new()));
    }
}

#[test]
#[ignore = "known gap: an empty answer blames ignore rules for hits removed by filters"]
fn filtered_hits_do_not_suggest_ignore_flags() {
    let late = format!("{}fn late() {{ marker(); }}\n", "// marker\n".repeat(3));
    let f = Fixture::new(&[
        (
            "src/auto.rs",
            "// @generated DO NOT EDIT\npub fn generatedneedle() {}\n",
        ),
        ("src/late.rs", &late),
    ]);
    f.indexed();
    for args in [
        vec!["generatedneedle", "--no-generated"],
        vec!["marker", "--kind", "string"],
    ] {
        let o = f.run(&args);
        assert!(!stdout(&o).contains("--no-ignore"), "{args:?}: {o:?}");
    }
}

#[test]
#[ignore = "known gap: symbol verbs cap definitions at 256 even when unlimited"]
fn unlimited_definitions_are_complete() {
    let body: String = (0..300)
        .map(|i| format!("mod m{i} {{ pub fn crowded() {{}} }}\n"))
        .collect();
    let f = Fixture::new(&[("many.rs", &body)]);
    f.indexed();
    for o in [
        f.run(&["def", "crowded", "--budget", "0", "--json"]),
        f.scan(&["def", "crowded", "--budget", "0", "--json"]),
    ] {
        let defs = stdout(&o).matches(r#""type":"def""#).count();
        assert_eq!(defs, 300, "{}", stdout(&o).lines().last().unwrap_or(""));
    }
}

#[test]
#[ignore = "known gap: JSON symbol verbs exit 0 without results"]
fn json_symbol_verbs_exit_1_without_results() {
    let f = Fixture::new(&[("a.rs", "fn x() {}\n")]);
    f.indexed();
    for verb in ["def", "refs", "callers", "impls", "impact"] {
        let text = f.run(&[verb, "zzz_nothing"]);
        let json = f.run(&[verb, "zzz_nothing", "--json"]);
        assert_eq!(text.status.code(), Some(1), "{verb} text");
        assert_eq!(json.status.code(), Some(1), "{verb} --json");
    }
}

#[test]
#[ignore = "known gap: the index omits hidden files a request asks for"]
fn hidden_files_requested_explicitly_are_found_through_the_index() {
    let f = Fixture::new(&[
        (".hidden.rs", "pub fn hiddenneedle() {}\n"),
        ("a.rs", "fn a() {}\n"),
    ]);
    f.indexed();
    for args in [
        vec!["def", "hiddenneedle", "--hidden"],
        vec!["hiddenneedle", "-g", ".hidden.rs"],
    ] {
        let scanned = f.scan(&args);
        assert_eq!(scanned.status.code(), Some(0), "scan {args:?}");
        let indexed = f.run(&args);
        assert_eq!(
            indexed.status.code(),
            Some(0),
            "index {args:?}: {indexed:?}"
        );
        assert!(stdout(&indexed).contains(".hidden.rs"), "{indexed:?}");
    }
}

#[cfg(unix)]
#[test]
#[ignore = "known gap: freshness follows a symlink that replaced an indexed file"]
fn a_symlink_replacing_an_indexed_file_is_not_followed() {
    let f = Fixture::with_filler(&[("a.txt", "plain\n")]);
    f.indexed();
    let outside = f.base.join("outside.txt");
    w(&outside, "symlinkneedle\n");
    fs::remove_file(f.root.join("a.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, f.root.join("a.txt")).unwrap();
    std::thread::sleep(PAST_FRESHNESS_WINDOW);
    let args = ["--fresh", "stat", "--budget", "0", "symlinkneedle"];
    assert_eq!(f.scan(&args).status.code(), Some(1));
    let o = f.run(&args);
    assert_eq!((o.status.code(), stdout(&o)), (Some(1), String::new()));
}

#[test]
#[ignore = "known gap: freshness compares only size and mtime"]
fn a_same_size_edit_with_restored_mtime_is_visible() {
    let f = Fixture::with_filler(&[("a.txt", "alpha_unique\n")]);
    f.indexed();
    let path = f.root.join("a.txt");
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    fs::write(&path, "bravo_unique\n").unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    std::thread::sleep(PAST_FRESHNESS_WINDOW);
    let o = f.run(&["--fresh", "stat", "--budget", "0", "bravo_unique"]);
    assert_eq!(
        (o.status.code(), stdout(&o)),
        (Some(0), "a.txt:1:bravo_unique\n".into())
    );
}

#[test]
#[ignore = "known gap: .git/info/exclude is not an index dependency"]
fn git_info_exclude_changes_the_indexed_file_set() {
    let f = Fixture::with_filler(&[("a.txt", "excludedneedle\n")]);
    f.indexed();
    w(&f.root.join(".git/info/exclude"), "a.txt\n");
    std::thread::sleep(PAST_FRESHNESS_WINDOW);
    let args = ["--fresh", "stat", "--budget", "0", "excludedneedle"];
    assert_eq!(f.scan(&args).status.code(), Some(1));
    let o = f.run(&args);
    assert_eq!((o.status.code(), stdout(&o)), (Some(1), String::new()));
}

#[test]
#[ignore = "known gap: resolver configuration edits leave import edges stale"]
fn a_resolver_config_edit_is_not_answered_from_stale_edges() {
    let tsconfig = |target: &str| {
        format!(r#"{{"compilerOptions":{{"baseUrl":".","paths":{{"@x":["{target}"]}}}}}}"#)
    };
    let f = Fixture::with_filler(&[
        ("tsconfig.json", &tsconfig("old.ts")),
        (
            "main.ts",
            "import { x } from \"@x\";\nexport const y = x;\n",
        ),
        ("old.ts", "export const x = 1;\n"),
        ("new.ts", "export const x = 2;\n"),
    ]);
    f.indexed();
    w(&f.root.join("tsconfig.json"), tsconfig("new.ts"));
    std::thread::sleep(PAST_FRESHNESS_WINDOW);
    let o = f.run(&["--fresh", "stat", "--json", "map", "."]);
    if o.status.code() == Some(2) {
        return; // an explicit "index unavailable" answer is acceptable
    }
    let imported_by = |file: &str| {
        stdout(&o)
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["type"] == "file" && v["data"]["path"] == file)
            .map(|v| v["data"]["imported_by"].as_u64().unwrap())
    };
    assert_eq!(
        (imported_by("old.ts"), imported_by("new.ts")),
        (Some(0), Some(1)),
        "{o:?}"
    );
}

#[test]
#[ignore = "known gap: a shape-valid dictionary corruption returns no hits"]
fn a_corrupted_dictionary_never_gives_an_authoritative_empty_answer() {
    let f = Fixture::new(&[("a.txt", "alpha_unique\n")]);
    f.indexed();
    let mut mutated = 0;
    for entry in fs::read_dir(&f.index).unwrap() {
        let p = entry.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with("words") && name.ends_with(".bin") {
            let bytes = fs::read(&p).unwrap();
            let Some(at) = bytes.windows(12).position(|w| w == b"alpha_unique") else {
                continue;
            };
            let mut bytes = bytes;
            bytes[at] = b'b';
            fs::write(&p, bytes).unwrap();
            mutated += 1;
        }
    }
    assert!(mutated > 0, "no dictionary holds the word");
    let o = f.run(&["--budget", "0", "-w", "alpha_unique"]);
    assert_ne!(
        o.status.code(),
        Some(1),
        "authoritative empty answer: {o:?}"
    );
    if o.status.code() == Some(0) {
        assert_eq!(stdout(&o), "a.txt:1:alpha_unique\n");
    }
}

#[cfg(unix)]
#[test]
#[ignore = "known gap: paths are converted lossily and backslashes become separators"]
fn a_backslash_in_a_file_name_is_preserved() {
    let f = Fixture::new(&[("a\\b.txt", "pathneedle\n")]);
    f.indexed();
    let args = ["--budget", "0", "pathneedle"];
    for o in [f.run(&args), f.scan(&args)] {
        assert_eq!(
            (o.status.code(), stdout(&o)),
            (Some(0), "a\\b.txt:1:pathneedle\n".into())
        );
    }
}

#[test]
#[ignore = "known gap: JSON output replaces invalid UTF-8 content"]
fn json_output_preserves_invalid_utf8_content() {
    let f = Fixture::new(&[]);
    w(&f.root.join("bad.txt"), b"inv\xffneedle\n");
    let o = f.scan(&["--json", "needle", "bad.txt"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(!stdout(&o).contains('\u{FFFD}'), "{}", stdout(&o));
}

#[test]
#[ignore = "known gap: detached builds ignore --index-dir"]
fn detached_builds_use_the_explicit_index_dir() {
    let f = Fixture::new(&[("a.rs", "fn needle() {}\n")]);
    let o = f
        .command()
        .env_remove("GREEG_INDEX_DIR")
        .args(["--no-session", "--index-dir"])
        .arg(&f.index)
        .arg("needle")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let manifest = f.index.join("manifest");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !manifest.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(manifest.exists(), "no index published in --index-dir");
    for default in ["home", "cache"] {
        let dir = f.base.join(default);
        assert!(
            !dir.exists(),
            "background build wrote under {}",
            dir.display()
        );
    }
}

#[cfg(unix)]
#[test]
#[ignore = "known gap: index files are created with default permissions"]
fn index_files_are_private_under_a_permissive_umask() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    let f = Fixture::new(&[("a.rs", "fn needle() {}\n")]);
    let mut c = f.command();
    c.args(["index", "--quiet", "--index-dir"]).arg(&f.index);
    // SAFETY: umask is async-signal-safe and touches only the child.
    unsafe {
        c.pre_exec(|| {
            libc::umask(0o022);
            Ok(())
        });
    }
    assert!(c.output().unwrap().status.success());
    let mut stack = vec![f.index.clone()];
    while let Some(p) = stack.pop() {
        let mode = fs::symlink_metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o077, 0, "{} is {mode:o}", p.display());
        if p.is_dir() {
            stack.extend(fs::read_dir(&p).unwrap().map(|e| e.unwrap().path()));
        }
    }
}
