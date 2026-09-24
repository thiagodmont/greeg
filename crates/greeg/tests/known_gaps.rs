//! Reproduced defects. Each test asserts the intended behavior; an open
//! defect's test is ignored, and CI fails when an ignored test here starts
//! passing, so a fix must remove its `#[ignore]`. Fixed ones stay as
//! regressions.
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
            .env("XDG_CONFIG_HOME", self.base.join("config"))
            .env("GREEG_STATS", "0")
            .env("GREEG_INDEX_DIR", &self.index)
            .env_remove("GREEG_SESSION")
            .env_remove("RIPGREP_CONFIG_PATH");
        c
    }

    /// This layout's index files, inside the chosen index directory.
    fn layout(&self) -> PathBuf {
        greeg_index::format_dir(&self.index)
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
fn indexed_implementors_and_map_honor_request_filters() {
    let f = Fixture::new(&[
        (
            "src/lib.rs",
            "pub trait Shape {}\npub struct A;\nimpl Shape for A {}\n",
        ),
        ("src/b.rs", "pub struct B;\nimpl crate::Shape for B {}\n"),
        ("tests/t.rs", "pub struct T;\nimpl Shape for T {}\n"),
    ]);
    f.indexed();
    for (flags, want) in [
        (vec!["--no-tests"], vec!["src/b.rs", "src/lib.rs"]),
        (vec!["-g", "!src/b.rs"], vec!["src/lib.rs", "tests/t.rs"]),
    ] {
        let mut args = vec!["impls", "Shape", "--json"];
        args.extend(&flags);
        let out = stdout(&f.run(&args));
        let mut got: Vec<&str> = want.iter().copied().filter(|w| out.contains(w)).collect();
        got.sort();
        assert_eq!(got, want, "impls {flags:?}: {out}");
        let excluded = ["src/b.rs", "src/lib.rs", "tests/t.rs"]
            .into_iter()
            .find(|p| !want.contains(p))
            .unwrap();
        assert!(!out.contains(excluded), "impls {flags:?}: {out}");

        let mut args = vec!["map", "--json"];
        args.extend(&flags);
        let out = stdout(&f.run(&args));
        assert!(!out.contains(excluded), "map {flags:?}: {out}");
        assert!(want.iter().all(|w| out.contains(w)), "map {flags:?}: {out}");
    }
}

#[test]
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
    // ranked output finds it too, rather than blaming ignore rules
    let ranked = f.run(&["marker", "--kind", "call"]);
    assert_eq!(ranked.status.code(), Some(0), "{ranked:?}");
    assert!(!stdout(&ranked).contains("--no-ignore"), "{ranked:?}");
}

#[test]
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
fn kind_counts_and_multiline_noncode_agree_across_backends() {
    let late = format!("{}fn late() {{ marker(); }}\n", "// marker\n".repeat(70));
    let f = Fixture::new(&[
        ("src/late.rs", &late),
        (
            "doc.py",
            "def f():\n    \"\"\"\n    marker()\n    \"\"\"\n    marker()\n",
        ),
    ]);
    f.indexed();
    for (args, want) in [
        (vec!["-c", "marker"], vec!["doc.py:2", "src/late.rs:71"]),
        (
            vec!["-c", "marker", "--kind", "call"],
            vec!["doc.py:1", "src/late.rs:1"],
        ),
        (
            vec!["-c", "marker", "--kind", "comment"],
            vec!["src/late.rs:70"],
        ),
        (
            vec!["-c", "marker", "--kind", "docstring"],
            vec!["doc.py:1"],
        ),
    ] {
        for o in [f.run(&args), f.scan(&args)] {
            assert_eq!(listed(&o), want, "{args:?}: {o:?}");
        }
    }
}

#[test]
fn a_definition_after_many_uses_is_shown_by_both_backends() {
    let body = format!("{}fn marker() {{}}\n", "// marker\n".repeat(70));
    let f = Fixture::new(&[("src/late.rs", &body)]);
    f.indexed();
    for o in [f.run(&["marker", "--json"]), f.scan(&["marker", "--json"])] {
        assert!(stdout(&o).contains(r#""line_number":71"#), "{o:?}");
    }
}

#[test]
fn a_rerun_after_sigbus_keeps_arguments_after_a_double_dash() {
    let f = Fixture::new(&[("a.txt", "-dashneedle here\n")]);
    f.indexed();
    let o = f
        .command()
        .env("GREEG_DEBUG_SIGBUS", "1")
        .args(["--no-session", "--index-dir"])
        .arg(&f.index)
        .args(["--", "-dashneedle"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(stdout(&o).contains("a.txt"), "{o:?}");
}

#[test]
fn a_relaxed_answer_exits_1_in_every_search_verb() {
    let f = Fixture::new(&[(
        "a.rs",
        "pub fn load_config() {}\nfn b() { load_config(); }\n",
    )]);
    f.indexed();
    for verb in [
        vec![],
        vec!["def"],
        vec!["refs"],
        vec!["callers"],
        vec!["impact"],
    ] {
        for name in ["load_confiq", "LOAD_CONFIG"] {
            let mut args = verb.clone();
            args.push(name);
            let o = f.run(&args);
            // the relaxed answer is still printed, and says what matched
            assert_eq!(o.status.code(), Some(1), "{args:?}: {o:?}");
            let out = stdout(&o);
            assert!(
                out.contains("load_config") || out.contains("matched "),
                "{args:?}: {o:?}"
            );
        }
        let mut args = verb.clone();
        args.push("load_config");
        assert_eq!(f.run(&args).status.code(), Some(0), "{args:?}");
    }
}

#[test]
fn json_footers_carry_one_outcome() {
    let f = Fixture::new(&[(
        "a.rs",
        "pub fn load_config() {}\nfn b() { load_config(); }\n",
    )]);
    f.indexed();
    for args in [
        vec!["load_config", "--json"],
        vec!["def", "load_config", "--json"],
        vec!["refs", "load_config", "--json"],
        vec!["callers", "load_config", "--json"],
        vec!["impls", "Nothing", "--json"],
        vec!["impact", "load_config", "--json"],
    ] {
        let o = f.run(&args);
        let footer: serde_json::Value = stdout(&o)
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .rfind(|v| v["type"] == "footer")
            .unwrap_or_else(|| panic!("{args:?}: {o:?}"));
        let oc = &footer["data"]["outcome"];
        assert_eq!(oc["exit"], o.status.code().unwrap(), "{args:?}: {footer}");
        assert!(
            oc["total"].as_u64().unwrap() >= oc["shown"].as_u64().unwrap(),
            "{footer}"
        );
        assert_eq!(oc["complete"], oc["total"] == oc["shown"], "{footer}");
        for k in ["exact", "rung", "source", "fresh"] {
            assert!(!oc[k].is_null(), "{args:?}: {k} in {footer}");
        }
    }
}

#[test]
fn output_that_cannot_be_written_fails_in_every_format() {
    let body: String = (0..5000)
        .map(|i| format!("fn f{i}() {{ needle(); }}\n"))
        .collect();
    let f = Fixture::new(&[("a.rs", &body)]);
    f.indexed();
    for args in [
        vec!["needle", "--budget", "0"],
        vec!["needle", "--json"],
        vec!["-n", "needle"],
        vec!["refs", "needle", "--budget", "0"],
    ] {
        let mut c = f.command();
        c.args(&args)
            .args(["--no-session", "--index-dir"])
            .arg(&f.index)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        drop(child.stdout.take());
        let o = child.wait_with_output().unwrap();
        // an answer that was not delivered is never reported as success
        assert_eq!(o.status.code(), Some(2), "{args:?}: {o:?}");
        assert!(!o.stderr.is_empty(), "{args:?}: {o:?}");
    }
}

#[test]
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
fn filtered_hits_do_not_suggest_ignore_flags() {
    let f = Fixture::new(&[
        (
            "src/auto.rs",
            "// @generated DO NOT EDIT\npub fn generatedneedle() {}\n",
        ),
        ("tests/t.rs", "fn t() { testneedle(); }\n"),
        ("src/c.rs", "// commentneedle\n"),
    ]);
    f.indexed();
    for args in [
        vec!["generatedneedle", "--no-generated"],
        vec!["testneedle", "--no-tests"],
        vec!["testneedle", "-t", "py"],
        vec!["commentneedle", "--kind", "call"],
    ] {
        for o in [f.run(&args), f.scan(&args)] {
            assert_eq!(o.status.code(), Some(1), "{args:?}: {o:?}");
            assert!(!stdout(&o).contains("--no-ignore"), "{args:?}: {o:?}");
            // the kind filter, not the ignore rules, removed the match
            if args.contains(&"--kind") {
                let all = format!("{}{}", stdout(&o), String::from_utf8_lossy(&o.stderr));
                assert!(all.contains("none of the requested kinds"), "{o:?}");
            }
        }
    }
}

#[test]
fn unlimited_definitions_are_complete() {
    let body: String = (0..300)
        .map(|i| format!("mod m{i} {{\n    pub fn crowded() {{}}\n}}\n"))
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
fn unlimited_implementations_are_complete_and_totals_are_eligible() {
    let body: String = (0..250)
        .map(|i| format!("pub struct S{i};\nimpl Crowd for S{i} {{}}\n"))
        .collect();
    let f = Fixture::new(&[
        ("src/lib.rs", "pub trait Crowd {}\n"),
        ("src/many.rs", &body),
    ]);
    f.indexed();
    let json = stdout(&f.run(&["impls", "Crowd", "--budget", "0", "--json"]));
    assert_eq!(
        json.matches(r#""confidence":"high""#).count(),
        250,
        "{json}"
    );
    assert!(json.contains(r#""direct":250"#), "{json}");
    // a budget limits what is shown, never the total
    let text = stdout(&f.run(&["impls", "Crowd"]));
    assert!(text.contains("250 implementations"), "{text}");

    // the total counts what the kind selects, not every definition of the name
    let def = stdout(&f.run(&["def", "S7", "--def-kind", "struct", "--json"]));
    assert!(def.contains(r#""total":1"#), "{def}");
}

#[test]
fn scan_definitions_nested_on_one_line_are_found() {
    let f = Fixture::new(&[("one.rs", "mod m { pub fn nested() {} }\n")]);
    f.indexed();
    for o in [f.run(&["def", "nested"]), f.scan(&["def", "nested"])] {
        assert_eq!(o.status.code(), Some(0), "{o:?}");
        assert!(stdout(&o).contains("one.rs"), "{o:?}");
    }
}

#[test]
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

/// Sorted `-l` output.
fn listed(o: &Output) -> Vec<String> {
    let mut v: Vec<String> = stdout(o).lines().map(str::to_string).collect();
    v.sort();
    v
}

fn skipping_fixture() -> Fixture {
    Fixture::new(&[
        (".gitignore", "target/\n*.log\nsrc/gen.rs\n.env\n"),
        ("src/lib.rs", "fn a() { skipneedle(); }\n"),
        ("src/gen.rs", "fn skipneedle() {}\n"),
        (".hidden.rs", "fn skipneedle() {}\n"),
        (".env", "skipneedle\n"),
        ("x.log", "skipneedle\n"),
        ("target/t.rs", "skipneedle\n"),
        (".github/w.yml", "skipneedle\n"),
    ])
}

const SKIPPING_REQUESTS: &[&[&str]] = &[
    &[],
    &["-g", "*.rs"],
    &["-t", "rust"],
    &["-g", "*.log"],
    &["-g", ".env"],
    &["-g", "*"],
    &["-g", "!src/**"],
    &["-t", "rust", "-g", "!.hidden.rs"],
    &["target"],
    &[".github"],
    &["--hidden"],
];

fn assert_matches_scan(f: &Fixture, extra: &[&str]) {
    for req in SKIPPING_REQUESTS {
        let mut args = vec!["-l", "skipneedle"];
        args.extend(*req);
        let scanned = listed(&f.scan(&args));
        assert_eq!(listed(&f.run(&args)), scanned, "{args:?}");
        if extra.is_empty() {
            continue;
        }
        // what was added after the build is found the same way
        let mut args = vec!["-l", "addedneedle"];
        args.extend(*req);
        assert_eq!(listed(&f.run(&args)), listed(&f.scan(&args)), "{args:?}");
    }
    for want in extra {
        let args = ["-l", "addedneedle", "-g", want];
        assert_eq!(listed(&f.run(&args)), vec![want.to_string()], "{args:?}");
    }
}

#[test]
fn requests_that_select_skipped_files_answer_like_a_scan() {
    let f = skipping_fixture();
    f.indexed();
    let expect = |args: &[&str], want: &[&str]| {
        let mut a = vec!["-l", "skipneedle"];
        a.extend(args);
        assert_eq!(listed(&f.scan(&a)), want, "scan {a:?}");
    };
    // ripgrep: a positive glob selects hidden and ignored files, a type hidden ones
    expect(&["-g", "*.rs"], &[".hidden.rs", "src/gen.rs", "src/lib.rs"]);
    expect(&["-t", "rust"], &[".hidden.rs", "src/lib.rs"]);
    expect(&["target"], &["target/t.rs"]);
    assert_matches_scan(&f, &[]);

    // skipped entries created after the build: first answered while the
    // refresh is pending, then from the published delta
    w(&f.root.join(".added.rs"), "fn addedneedle() {}\n");
    w(&f.root.join("added.log"), "addedneedle\n");
    std::thread::sleep(PAST_FRESHNESS_WINDOW);
    assert_matches_scan(&f, &[".added.rs", "added.log"]);
    std::thread::sleep(Duration::from_secs(1));
    assert_matches_scan(&f, &[".added.rs", "added.log"]);
}

#[test]
fn symbol_verbs_find_definitions_in_selected_skipped_files() {
    let f = skipping_fixture();
    f.indexed();
    for args in [
        vec!["def", "skipneedle", "-g", ".hidden.rs"],
        vec!["def", "skipneedle", "-g", "src/gen.rs"],
        vec!["def", "skipneedle", "--no-ignore"],
    ] {
        let o = f.run(&args);
        assert_eq!(o.status.code(), Some(0), "{args:?}: {o:?}");
        let want = if args.contains(&"--no-ignore") {
            "src/gen.rs"
        } else {
            args[3]
        };
        assert!(stdout(&o).contains(want), "{args:?}: {o:?}");
    }
    // a type reaches the hidden file, not the ignored one; the index still
    // answers for everything else
    let def = stdout(&f.run(&["def", "skipneedle", "-t", "rust", "--budget", "0"]));
    assert!(
        def.contains(".hidden.rs") && !def.contains("src/gen.rs"),
        "{def}"
    );
    let map = stdout(&f.run(&["map", "-t", "rust", "--json"]));
    assert!(
        map.contains(".hidden.rs") && !map.contains("src/gen.rs"),
        "{map}"
    );
    let map = f.run(&["map", "--hidden"]);
    assert_eq!(map.status.code(), Some(2), "{map:?}");
}

#[test]
fn an_index_without_a_skipped_record_is_not_trusted_to_cover_a_request() {
    let f = skipping_fixture();
    f.indexed();
    let manifest = f.layout().join("manifest");
    let mut m: serde_json::Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    m.as_object_mut().unwrap().remove("skipped");
    fs::write(&manifest, m.to_string()).unwrap();
    let args = ["-l", "skipneedle", "-g", "*.rs"];
    assert_eq!(
        listed(&f.run(&args)),
        [".hidden.rs", "src/gen.rs", "src/lib.rs"]
    );
}

#[cfg(unix)]
#[test]
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
    // a symlink added after the build is not indexed either
    std::os::unix::fs::symlink(&outside, f.root.join("added.txt")).unwrap();
    std::thread::sleep(PAST_FRESHNESS_WINDOW);
    let o = f.run(&args);
    assert_eq!((o.status.code(), stdout(&o)), (Some(1), String::new()));
}

#[cfg(unix)]
#[test]
fn a_fifo_replacing_an_indexed_file_does_not_block_the_search() {
    let f = Fixture::with_filler(&[("a.txt", "fifoneedle\n")]);
    f.indexed();
    let path = f.root.join("a.txt");
    fs::remove_file(&path).unwrap();
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    std::thread::sleep(PAST_FRESHNESS_WINDOW);
    let mut child = f
        .command()
        .args([
            "--fresh",
            "stat",
            "--budget",
            "0",
            "--no-session",
            "--index-dir",
        ])
        .arg(&f.index)
        .arg("fifoneedle")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("search blocked on a FIFO");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let o = child.wait_with_output().unwrap();
    assert_eq!((o.status.code(), stdout(&o)), (Some(1), String::new()));
}

#[cfg(unix)]
#[test]
#[ignore = "known gap: reads follow a symlink that replaces an indexed file's directory"]
fn a_symlinked_ancestor_directory_is_not_followed() {
    let f = Fixture::with_filler(&[("src/a.rs", "fn plain() {}\n")]);
    f.indexed();
    let outside = f.base.join("outside");
    w(&outside.join("a.rs"), "fn ancestorneedle() {}\n");
    fs::remove_dir_all(f.root.join("src")).unwrap();
    std::os::unix::fs::symlink(&outside, f.root.join("src")).unwrap();
    std::thread::sleep(PAST_FRESHNESS_WINDOW);
    for args in [
        &["--fresh", "stat", "--budget", "0", "ancestorneedle"][..],
        &["--fresh", "stat", "def", "ancestorneedle"],
    ] {
        assert_eq!(f.scan(args).status.code(), Some(1), "{args:?}");
        let o = f.run(args);
        assert_eq!(
            (o.status.code(), stdout(&o).contains("ancestorneedle")),
            (Some(1), false),
            "{args:?}: {o:?}"
        );
    }
}

#[test]
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

/// An editor's save-by-rename brings a new inode even when size and mtime
/// match the old file.
#[test]
fn an_atomic_replace_with_the_same_size_and_mtime_is_visible() {
    let f = Fixture::with_filler(&[("a.txt", "alpha_unique\n")]);
    f.indexed();
    let path = f.root.join("a.txt");
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let tmp = f.root.join("a.txt.new");
    fs::write(&tmp, "bravo_unique\n").unwrap();
    fs::File::options()
        .write(true)
        .open(&tmp)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    fs::rename(&tmp, &path).unwrap();
    std::thread::sleep(PAST_FRESHNESS_WINDOW);
    let o = f.run(&["--fresh", "stat", "--budget", "0", "bravo_unique"]);
    assert_eq!(
        (o.status.code(), stdout(&o)),
        (Some(0), "a.txt:1:bravo_unique\n".into())
    );
}

#[test]
fn git_info_exclude_changes_the_indexed_file_set() {
    let f = Fixture::with_filler(&[("a.txt", "excludedneedle\n"), ("b.txt", "globalneedle\n")]);
    f.indexed();
    let search = |pattern: &str| {
        std::thread::sleep(PAST_FRESHNESS_WINDOW);
        let args = ["--fresh", "stat", "--budget", "0", pattern];
        let (o, scanned) = (f.run(&args), f.scan(&args));
        assert_eq!(o.status.code(), scanned.status.code(), "{pattern}: {o:?}");
        stdout(&o)
    };
    let exclude = f.root.join(".git/info/exclude");
    w(&exclude, "a.txt\n");
    assert_eq!(search("excludedneedle"), "");
    fs::remove_file(&exclude).unwrap();
    assert_eq!(search("excludedneedle"), "a.txt:1:excludedneedle\n");
    w(&f.base.join("config/git/ignore"), "b.txt\n");
    assert_eq!(search("globalneedle"), "");
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
#[ignore = "known gap: graph verbs exit 2 while a rebuild runs"]
fn graph_verbs_wait_for_a_short_rebuild() {
    let f = Fixture::with_filler(&[
        ("a.rs", "pub fn alpha() {}\n"),
        ("b.rs", "pub fn bravo() {}\n"),
    ]);
    f.indexed();
    // an ignore-input change needs a rebuild; this tree's is well under the
    // wait, so the answer must come from the new index, not exit 2
    w(&f.root.join(".git/info/exclude"), "b.rs\n");
    std::thread::sleep(PAST_FRESHNESS_WINDOW);
    let o = f.run(&["--fresh", "stat", "map", "."]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let out = stdout(&o);
    assert!(out.contains("a.rs") && !out.contains("b.rs"), "{out}");
}

#[test]
#[ignore = "known gap: a shape-valid dictionary corruption returns no hits"]
fn a_corrupted_dictionary_never_gives_an_authoritative_empty_answer() {
    let f = Fixture::new(&[("a.txt", "alpha_unique\n")]);
    f.indexed();
    let mut mutated = 0;
    for entry in fs::read_dir(f.layout()).unwrap() {
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
    let manifest = f.layout().join("manifest");
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
fn index_files_are_private_under_a_permissive_umask() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    let permissive = |mut c: Command| {
        // SAFETY: umask is async-signal-safe and touches only the child.
        unsafe {
            c.pre_exec(|| {
                libc::umask(0o022);
                Ok(())
            });
        }
        c.output().unwrap()
    };
    let private_below = |dir: &Path, root_mode: u32| {
        let mode = |p: &Path| {
            fs::symlink_metadata(p)
                .ok()
                .map(|m| m.permissions().mode() & 0o777)
        };
        assert_eq!(mode(dir), Some(root_mode), "{}", dir.display());
        let mut stack: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        while let Some(p) = stack.pop() {
            // a temporary file can be renamed away between listing and stat
            let Some(m) = mode(&p) else { continue };
            assert_eq!(m & 0o077, 0, "{} is {m:o}", p.display());
            if let Ok(entries) = fs::read_dir(&p) {
                stack.extend(entries.flatten().map(|e| e.path()));
            }
        }
    };
    let f = Fixture::with_filler(&[("a.rs", "fn needle() {}\n")]);
    let mut c = f.command();
    c.args(["index", "--quiet", "--index-dir"]).arg(&f.index);
    assert!(permissive(c).status.success());
    // an edit publishes a delta segment in its own directory
    std::thread::sleep(PAST_FRESHNESS_WINDOW);
    w(&f.root.join("a.rs"), "fn needle() { edited(); }\n");
    let mut c = f.command();
    c.args(["--fresh", "stat", "--no-session", "--index-dir"])
        .arg(&f.index)
        .arg("edited");
    assert_eq!(permissive(c).status.code(), Some(0));
    // wait for the detached refresh to publish the delta and finish
    let published = || {
        greeg_index::read_manifest(&f.layout()).is_some_and(|m| m.deltas > 0)
            && !f.layout().join("REFRESHING").exists()
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while !published() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(published(), "no delta published");
    private_below(&f.index, 0o700);

    // an existing directory someone else chose is used, not chmodded
    let chosen = f.base.join("chosen");
    fs::create_dir(&chosen).unwrap();
    fs::set_permissions(&chosen, fs::Permissions::from_mode(0o755)).unwrap();
    let mut c = f.command();
    c.args(["index", "--quiet", "--index-dir"]).arg(&chosen);
    assert!(permissive(c).status.success());
    private_below(&chosen, 0o755);
}
