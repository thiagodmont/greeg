//! CLI contract and scan/index parity tests using isolated local fixtures.
//! No external corpus or ripgrep installation is required.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_greeg");
static N: AtomicU32 = AtomicU32::new(0);

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

fn w(p: &Path, body: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// A tree small enough to reason about and varied enough to exercise the
/// contract: two languages, a demoted test file, a vendored file, a generated
/// file, a binary file, an ignored directory, and identifiers that are
/// prefixes of one another (for `related`).
fn fixture() -> Fixture {
    let f = empty_fixture();
    let root = &f.root;

    w(
        &root.join("src/handler.rs"),
        "use crate::util::load_config;\n\
         \n\
         /// Handle one request.\n\
         pub fn handle_request(id: u32) -> u32 {\n\
         \x20   let cfg = load_config();\n\
         \x20   id + cfg\n\
         }\n\
         \n\
         pub fn handle_request_batch(ids: &[u32]) -> u32 {\n\
         \x20   ids.iter().map(|&i| handle_request(i)).sum()\n\
         }\n",
    );
    w(
        &root.join("src/util.rs"),
        "pub fn load_config() -> u32 {\n\
         \x20   // load_config reads nothing yet\n\
         \x20   7\n\
         }\n",
    );
    w(
        &root.join("app/views.py"),
        "def handle_request(request):\n\
         \x20   \"\"\"handle_request docstring.\"\"\"\n\
         \x20   return request\n",
    );
    w(
        &root.join("tests/test_handler.rs"),
        "#[test]\nfn handle_request_works() {\n    handle_request(1);\n}\n",
    );
    w(
        &root.join("vendor/lib/other.rs"),
        "pub fn handle_request() {}\n",
    );
    w(
        &root.join("src/generated/api.rs"),
        "// @generated DO NOT EDIT\npub fn handle_request() {}\n",
    );
    w(
        &root.join("docs/notes.md"),
        "handle_request is the entry.\n",
    );
    fs::write(root.join("bin.dat"), b"\x00\x01handle_request\x00").unwrap();
    w(&root.join("skipped/hidden.rs"), "fn handle_request() {}\n");
    w(&root.join(".gitignore"), "skipped/\n");
    f
}

/// An empty git work tree in a directory this fixture creates itself, so
/// `Drop` never removes one left behind by another run.
fn empty_fixture() -> Fixture {
    let base = loop {
        let base = std::env::temp_dir().join(format!(
            "greeg-cli-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&base) {
            Ok(()) => break base,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("create fixture: {e}"),
        }
    };
    let root = base.join("tree");
    let index = base.join("index");
    // `.gitignore` only applies inside a git repository, as in ripgrep
    fs::create_dir_all(root.join(".git")).unwrap();
    Fixture { base, root, index }
}

impl Fixture {
    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .arg("--no-session")
            .arg("--index-dir")
            .arg(&self.index)
            .current_dir(&self.root)
            .env("GREEG_STATS", "0")
            .env("GREEG_INDEX_DIR", &self.index)
            .output()
            .expect("run greeg")
    }

    /// stdout of a successful run, as text.
    fn out(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(
            o.status.code() == Some(0) || o.status.code() == Some(1),
            "greeg {args:?} exited {:?}\nstderr: {}",
            o.status.code(),
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn err(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&self.run(args).stderr).into_owned()
    }

    /// Build the index and wait for it, so a test that wants the index path
    /// is not racing the background build.
    fn indexed(&self) -> &Self {
        let o = Command::new(BIN)
            .args(["index", "--quiet", "--index-dir"])
            .arg(&self.index)
            .current_dir(&self.root)
            .env("GREEG_STATS", "0")
            .output()
            .expect("build index");
        assert!(o.status.success(), "index: {:?}", o.status);
        self
    }
}

/// `path:line` pairs from a parity-mode answer (`--budget 0`).
fn pairs(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, ':');
            let p = it.next()?;
            let n = it.next()?;
            n.parse::<u32>().ok()?;
            Some(format!("{p}:{n}"))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// the property: the index must not change the answer
// ---------------------------------------------------------------------------

/// The single assertion that catches most index bugs without pinning any
/// text: whatever the index does to candidate selection, ranking or freshness,
/// the set of matching lines must equal what a tree scan finds.
#[test]
fn an_indexed_answer_matches_a_scanned_one() {
    let f = fixture();
    f.indexed();
    for args in [
        vec!["handle_request"],
        vec!["-w", "handle_request"],
        vec!["-i", "HANDLE_REQUEST"],
        vec!["load_config"],
        vec!["-F", "fn handle_request(id: u32)"],
        vec!["handle_\\w+"],
        vec!["-t", "rs", "handle_request"],
        vec!["-g", "src/**", "handle_request"],
        vec!["--no-tests", "handle_request"],
        vec!["-x", "}"],
        vec!["nothing_matches_this"],
    ] {
        let mut indexed = args.clone();
        indexed.extend(["--budget", "0", "--no-ladder"]);
        let mut scanned = indexed.clone();
        scanned.push("--no-index");
        assert_eq!(
            pairs(&f.out(&indexed)),
            pairs(&f.out(&scanned)),
            "indexed and scanned answers differ for {args:?}"
        );
    }
}

/// The same property across an edit: the freshness check must fold the
/// changed file in, whether or not the index was built before it.
#[test]
fn an_edit_is_visible_to_an_indexed_search() {
    let f = fixture();
    f.indexed();
    w(
        &f.root.join("src/late.rs"),
        "pub fn handle_request_late() -> u32 { 1 }\n",
    );
    let out = f.out(&["--budget", "0", "--no-ladder", "handle_request_late"]);
    assert!(out.contains("src/late.rs"), "new file missing:\n{out}");
    assert_eq!(
        pairs(&out),
        pairs(&f.out(&[
            "--budget",
            "0",
            "--no-ladder",
            "--no-index",
            "handle_request_late"
        ])),
    );
}

// ---------------------------------------------------------------------------
// footer contract
// ---------------------------------------------------------------------------

/// "A whole answer ... ends with just `N hits · M files`".
#[test]
fn a_complete_answer_has_a_terse_footer() {
    let f = fixture();
    let out = f.out(&["load_config", "src/util.rs"]);
    let footer = out
        .lines()
        .rfind(|l| l.contains(" hits · "))
        .unwrap_or_else(|| panic!("no footer in:\n{out}"));
    assert!(
        !footer.contains('/'),
        "a complete answer must not print shown/total: {footer:?}"
    );
    assert!(
        !footer.contains("tokens"),
        "a complete answer must not print the token estimate: {footer:?}"
    );
    assert!(
        !footer.contains("demoted"),
        "nothing was demoted here: {footer:?}"
    );
}

/// "`no hits` is bare" — when nothing was skipped. The fixture holds a binary
/// file, so the honest footer names it; searching only the text tree gives the
/// bare line. Both shapes are pinned here because the difference is the whole
/// rule.
#[test]
fn an_empty_answer_is_bare() {
    let f = fixture();
    // A ready index must not change these scan-specific footer assertions.
    f.indexed();
    let o = f.run(&["--no-index", "--no-ladder", "zzz_no_such_identifier", "src"]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert_eq!(o.status.code(), Some(1), "no match must exit 1");
    assert_eq!(
        out.lines().find(|l| !l.is_empty()).unwrap_or(""),
        "no hits",
        "with nothing skipped, the footer is the bare line, got:\n{out}"
    );
    assert!(
        !out.contains("escalation ladder"),
        "--no-ladder means the ladder never ran, so it cannot have failed:\n{out}"
    );
    // with a binary file in scope the footer says so, and says it once: the
    // zero terms are dropped
    let whole = f.out(&["--no-index", "--no-ladder", "zzz_no_such_identifier"]);
    let first = whole.lines().find(|l| !l.is_empty()).unwrap();
    assert_eq!(first, "no hits · skipped 1 binary", "{whole}");
}

/// When the budget cuts the answer, the footer must say so rather than look
/// complete: shown/total, and the token estimate.
#[test]
fn a_truncated_answer_says_what_was_cut() {
    let f = fixture();
    let out = f.out(&["--budget", "60", "handle_request"]);
    let footer = out
        .lines()
        .rfind(|l| l.contains(" hits · "))
        .unwrap_or_else(|| panic!("no footer in:\n{out}"));
    assert!(
        footer.contains('/'),
        "a cut answer must print shown/total: {footer:?}"
    );
    assert!(
        footer.contains("tokens"),
        "a cut answer must print the estimate: {footer:?}"
    );
}

/// Test, vendored and generated files are demoted, never hidden, and the
/// footer accounts for them.
#[test]
fn demoted_files_are_counted_not_hidden() {
    let f = fixture();
    let all = f.out(&["--budget", "0", "--no-ladder", "handle_request"]);
    for p in [
        "tests/test_handler.rs",
        "vendor/lib/other.rs",
        "src/generated/api.rs",
    ] {
        assert!(all.contains(p), "{p} must still be searched:\n{all}");
    }
    let footer = f.err(&["--budget", "0", "handle_request"]);
    assert!(
        footer.contains("demoted"),
        "the footer must account for the demoted files: {footer:?}"
    );
    // ...and --no-tests removes them from the answer entirely
    let no_tests = f.out(&[
        "--budget",
        "0",
        "--no-ladder",
        "--no-tests",
        "handle_request",
    ]);
    assert!(!no_tests.contains("tests/test_handler.rs"), "{no_tests}");
    assert!(no_tests.contains("src/handler.rs"), "{no_tests}");
}

/// A binary file is skipped and the footer says how many, rather than
/// silently answering as if the tree held only text.
#[test]
fn a_skipped_binary_file_is_reported() {
    let f = fixture();
    let out = f.out(&["handle_request"]);
    assert!(
        !out.contains("bin.dat"),
        "binary content must not print:\n{out}"
    );
    let footer = out.lines().rfind(|l| l.contains(" hits · ")).unwrap();
    assert!(
        footer.contains("skipped 1 binary"),
        "the footer must count the skipped binary: {footer:?}"
    );
}

// ---------------------------------------------------------------------------
// ranked layout
// ---------------------------------------------------------------------------

/// A row that *is* a definition links to its parent, never to itself:
/// `handle_request  ‹ handle_request` is noise.
#[test]
fn a_definition_row_never_names_itself() {
    let f = fixture();
    for line in f.out(&["handle_request"]).lines() {
        if let Some((row, container)) = line.split_once('‹') {
            let name = container.trim();
            assert!(
                !(row.contains(" def ") && row.contains(&format!("fn {name}"))),
                "a definition row names its own definition: {line:?}"
            );
        }
    }
}

/// A bare identifier answers about the whole word; the longer identifiers
/// that contain it are named on `related` instead of taking answer rows.
#[test]
fn a_bare_identifier_answers_about_the_whole_word() {
    let f = fixture();
    f.indexed();
    let out = f.out(&["handle_request"]);
    let related = out
        .lines()
        .find(|l| l.trim_start().starts_with("related"))
        .unwrap_or_else(|| panic!("no `related` line in:\n{out}"));
    assert!(
        related.contains("handle_request_batch"),
        "the longer identifier belongs on `related`: {related:?}"
    );
    assert!(
        !out.contains("handle_request_batch(ids"),
        "the longer identifier must not take an answer row:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// rg-shaped streams
// ---------------------------------------------------------------------------

/// `-l` and `-c` are pipe-safe: bare paths / `path:count` on stdout, footer
/// on stderr, never truncated by the budget. The hook rewrites
/// `rg -l … | xargs …`, so anything else on stdout breaks the pipe.
#[test]
fn files_and_count_modes_keep_ripgreps_shape() {
    let f = fixture();
    let l = f.out(&["-l", "handle_request"]);
    assert!(!l.is_empty());
    for line in l.lines() {
        assert!(
            f.root.join(line).exists(),
            "-l stdout must be bare paths, got {line:?}"
        );
    }
    assert!(
        f.err(&["-l", "handle_request"]).contains("hits"),
        "the -l footer belongs on stderr"
    );
    let c = f.out(&["-c", "handle_request"]);
    for line in c.lines() {
        let (p, n) = line.rsplit_once(':').unwrap_or_else(|| panic!("{line:?}"));
        assert!(f.root.join(p).exists(), "{line:?}");
        assert!(n.parse::<u32>().unwrap() > 0, "{line:?}");
    }
}

/// `--budget 0` is parity mode: `path:line:text` in path order, nothing else
/// on stdout.
#[test]
fn budget_zero_is_ripgrep_shaped_and_path_ordered() {
    let f = fixture();
    f.indexed();
    let out = f.out(&["--budget", "0", "--no-ladder", "handle_request"]);
    let paths: Vec<&str> = out.lines().filter_map(|l| l.split(':').next()).collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(paths, sorted, "parity mode is path-ordered:\n{out}");
    assert!(
        out.lines().all(|l| l
            .split(':')
            .nth(1)
            .is_some_and(|n| n.parse::<u32>().is_ok())),
        "every parity-mode row is path:line:text:\n{out}"
    );
}

// A nonzero exit cannot prevent a pipeline from consuming incorrect stdout.
#[test]
fn machine_modes_do_not_emit_relaxed_matches() {
    let f = fixture();
    f.indexed();
    for backend in [vec!["--no-index"], vec!["--fresh", "stat"]] {
        for mode in [
            vec!["-l"],
            vec!["-c"],
            vec!["--mode", "files"],
            vec!["--mode", "count"],
            vec!["--budget", "0"],
        ] {
            for query in [
                vec!["LOAD_CONFIG"],
                vec!["-w", "load_conf"],
                vec!["LoadConfig"],
                vec!["load_confiq"],
            ] {
                let args: Vec<_> = backend.iter().chain(&mode).chain(&query).copied().collect();
                let out = f.run(&args);
                assert_eq!(out.status.code(), Some(1), "{args:?}");
                assert!(out.stdout.is_empty(), "{args:?}: {:?}", out.stdout);
                assert!(
                    !String::from_utf8_lossy(&out.stderr).contains("after the escalation ladder")
                );
            }
        }
        for query in ["LOAD_CONFIG", "LoadConfig", "load_confiq"] {
            let mut args = backend.clone();
            args.extend(["--json", query]);
            let out = f.run(&args);
            assert_eq!(out.status.code(), Some(1), "{args:?}");
            let records: Vec<serde_json::Value> = String::from_utf8(out.stdout)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            assert!(!records.iter().any(|r| r["type"] == "match"), "{args:?}");
            let footer = records.iter().find(|r| r["type"] == "footer").unwrap();
            assert_eq!(footer["data"]["hits_total"], 0);
            assert_eq!(footer["data"]["rung"], "exact");
        }
    }
}

#[test]
fn matching_policy_is_explicit_and_keeps_the_legacy_opt_out() {
    let f = fixture();
    f.indexed();
    for backend in [vec!["--no-index"], vec!["--fresh", "stat"]] {
        let mut exact = backend.clone();
        exact.extend(["--matching", "exact", "LOAD_CONFIG"]);
        let out = f.run(&exact);
        assert_eq!(out.status.code(), Some(1));
        assert!(!String::from_utf8_lossy(&out.stdout).contains("src/util.rs"));

        let mut legacy = backend.clone();
        legacy.extend(["--no-ladder", "LOAD_CONFIG"]);
        assert_eq!(f.run(&legacy).stdout, out.stdout);

        for mode in [
            vec!["-l"],
            vec!["-c"],
            vec!["--budget", "0"],
            vec!["--json"],
        ] {
            let mut args = backend.clone();
            args.extend(mode);
            args.extend(["--matching", "discover", "LOAD_CONFIG"]);
            let out = f.run(&args);
            assert_eq!(out.status.code(), Some(1), "relaxed matches still exit 1");
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("src/util.rs"),
                "{args:?}"
            );
            let all = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(all.contains("case-insensitive"), "{args:?}: {all}");
        }
    }
    for args in [
        vec!["--matching", "discover", "--no-ladder", "load_config"],
        vec!["--no-ladder", "--matching", "exact", "load_config"],
        vec!["--matching", "unknown", "load_config"],
    ] {
        let out = f.run(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn exact_policy_preserves_requested_case_and_word_semantics() {
    let f = fixture();
    f.indexed();
    for backend in [vec!["--no-index"], vec!["--fresh", "stat"]] {
        for flags in [
            vec!["-i", "LOAD_CONFIG"],
            vec!["-w", "load_config"],
            vec!["-S", "load_config"],
        ] {
            let mut args = backend.clone();
            args.extend(flags);
            args.extend(["-l", "--sort", "path"]);
            let default = f.run(&args);
            assert_eq!(default.status.code(), Some(0), "{args:?}");
            args.extend(["--matching", "exact"]);
            assert_eq!(default.stdout, f.run(&args).stdout);
            args.pop();
            args.push("discover");
            assert_eq!(default.stdout, f.run(&args).stdout);
            assert_eq!(
                String::from_utf8(default.stdout).unwrap(),
                "src/handler.rs\nsrc/util.rs\n"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// exit codes and flags
// ---------------------------------------------------------------------------

#[test]
fn exit_codes_follow_ripgrep() {
    let f = fixture();
    assert_eq!(
        f.run(&["handle_request"]).status.code(),
        Some(0),
        "match → 0"
    );
    assert_eq!(
        f.run(&["--no-ladder", "zzz_no_such_identifier"])
            .status
            .code(),
        Some(1),
        "no match → 1"
    );
    assert_eq!(
        f.run(&["handle_request", "no/such/path.rs"]).status.code(),
        Some(2),
        "unreadable path → 2"
    );
    assert_eq!(f.run(&["("]).status.code(), Some(2), "bad regex → 2");
}

/// A flag greeg cannot honour must fail loudly rather than answer without it:
/// `-a`/`-uuu` ask for binary files the index never holds.
#[test]
fn flags_that_cannot_be_honoured_are_rejected() {
    let f = fixture();
    for args in [
        vec!["-a", "handle_request"],
        vec!["--text", "handle_request"],
        vec!["-uuu", "handle_request"],
    ] {
        let o = f.run(&args);
        assert_eq!(o.status.code(), Some(2), "{args:?} must exit 2");
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains("binary"), "{args:?} must say why: {err:?}");
    }
    // -u and -uu widen the file set, not the bytes, and stay supported
    let o = f.run(&["-uu", "--budget", "0", "--no-ladder", "handle_request"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("skipped/hidden.rs"),
        "-uu must reach the ignored directory"
    );
}

/// Cosmetic ripgrep flags are accepted and ignored, because the output they
/// ask for is the output greeg gives. The hook depends on this.
#[test]
fn cosmetic_flags_are_accepted() {
    let f = fixture();
    let plain = f.out(&["--budget", "0", "--no-ladder", "handle_request"]);
    for extra in [
        vec!["-N"],
        vec!["-H"],
        vec!["--no-heading"],
        vec!["--column"],
        vec!["--color", "never"],
        vec!["--trim"],
    ] {
        let mut args = extra.clone();
        args.extend(["--budget", "0", "--no-ladder", "handle_request"]);
        assert_eq!(f.out(&args), plain, "{extra:?} changed the answer");
    }
}

/// A file named on the command line is searched even when the ignore rules
/// hide it: ripgrep does not filter its roots, and neither does greeg.
#[test]
fn an_explicit_path_beats_the_ignore_rules() {
    let f = fixture();
    f.indexed();
    let out = f.out(&["--budget", "0", "handle_request", "skipped/hidden.rs"]);
    assert!(
        out.contains("skipped/hidden.rs"),
        "an explicitly named ignored file must be searched:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// verbs
// ---------------------------------------------------------------------------

/// `def` finds the definition and prefers the source one over the demoted
/// copies in tests, vendor and generated code.
#[test]
fn def_prefers_the_source_definition() {
    let f = fixture();
    f.indexed();
    let out = f.out(&["def", "load_config"]);
    assert!(out.contains("src/util.rs"), "{out}");
    let first = out
        .lines()
        .find(|l| l.contains(".rs") || l.contains(".py"))
        .unwrap_or_else(|| panic!("no rows in:\n{out}"));
    assert!(
        !first.contains("vendor/") && !first.contains("tests/"),
        "a demoted definition ranked first: {first:?}"
    );
}

#[test]
fn def_exact_honors_case_flags_on_both_backends() {
    let f = fixture();
    w(
        &f.root.join("src/case_variants.rs"),
        "pub fn LOAD_CONFIG() {}\npub fn café() {}\npub fn CAFÉ() {}\n",
    );
    w(
        &f.root.join("src/módulo.rs"),
        "//! Module without a named definition.\n",
    );
    f.indexed();
    for backend in [vec!["--no-index"], vec!["--fresh", "stat"]] {
        for (flags, name, expected) in [
            (vec!["-i"], "Load_Config", 2),
            (vec!["-i"], "load_config", 2),
            (vec!["-S"], "load_config", 2),
            (vec!["-S"], "LOAD_CONFIG", 1),
            (vec!["-i", "-s"], "Load_Config", 0),
            (vec!["-S", "-s"], "Load_Config", 0),
            (vec![], "load_config", 1),
            (vec!["-i"], "LoadConfig", 0),
            (vec!["-i"], "load_confiq", 0),
            (vec!["-i"], "café", 2),
            (vec!["-S"], "café", 2),
            (vec!["-S"], "CAFÉ", 1),
        ] {
            let mut args = vec!["def", name, "--matching", "exact"];
            args.extend(&backend);
            args.extend(flags);
            let out = f.run(&args);
            assert_eq!(
                out.status.code(),
                Some(if expected == 0 { 1 } else { 0 }),
                "{args:?}"
            );
            assert!(out.stderr.is_empty(), "{args:?}: {:?}", out.stderr);
            args.push("--json");
            let out = f.run(&args);
            let records: Vec<serde_json::Value> = String::from_utf8(out.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                records.iter().filter(|r| r["type"] == "def").count(),
                expected,
                "{args:?}: {records:?}"
            );
            let footer = &records.last().unwrap()["data"];
            assert_eq!(footer["rung"], "exact", "{args:?}");
            assert_eq!(footer["suggestions"], serde_json::json!([]));
            assert_eq!(
                footer["source"],
                if backend[0] == "--no-index" || !name.is_ascii() {
                    "scan"
                } else {
                    "index"
                }
            );
        }
    }
    let module = f.run(&["def", "módulo", "--matching", "exact", "--json"]);
    assert!(String::from_utf8_lossy(&module.stdout).contains("src/módulo.rs"));
}

/// `show FILE:LINE` prints the definition enclosing a line, whole — the read
/// the agent would otherwise guess at with `sed -n`.
#[test]
fn show_prints_the_enclosing_definition() {
    let f = fixture();
    f.indexed();
    let out = f.out(&["show", "src/handler.rs:6"]);
    assert!(out.contains("pub fn handle_request(id: u32)"), "{out}");
    assert!(out.contains("id + cfg"), "the whole body:\n{out}");
    assert!(
        !out.contains("handle_request_batch"),
        "only the enclosing definition:\n{out}"
    );
}

/// `refs` groups by kind, and the call in another file is one of them.
#[test]
fn refs_finds_the_call_site() {
    let f = fixture();
    f.indexed();
    let out = f.out(&["refs", "load_config"]);
    assert!(
        out.contains("src/handler.rs"),
        "the caller is missing:\n{out}"
    );
    assert!(
        out.contains("src/util.rs"),
        "the definition is missing:\n{out}"
    );
}

/// The escalation ladder reports the rung it used rather than answering as if
/// the query had matched as asked.
#[test]
fn the_ladder_reports_the_rung_it_used() {
    let f = fixture();
    f.indexed();
    let out = f.out(&["LOAD_CONFIG"]);
    assert!(
        out.contains("src/util.rs"),
        "the ladder should find it:\n{out}"
    );
    assert!(
        out.contains("case-insensitive"),
        "the footer must name the rung:\n{out}"
    );
}

/// stdin is searched like ripgrep when it is a pipe, with no footer.
#[test]
fn stdin_is_searched_like_ripgrep() {
    use std::io::Write;
    use std::process::Stdio;
    let mut c = Command::new(BIN)
        .args(["--no-session", "alpha"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GREEG_STATS", "0")
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(b"alpha\nbeta\n").unwrap();
    let o = c.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "alpha");
}

#[test]
fn stdin_does_not_silently_accept_discovery() {
    use std::io::Write;
    use std::process::Stdio;
    for flags in [
        vec![],
        vec!["--matching", "exact"],
        vec!["--matching", "discover"],
    ] {
        let mut child = Command::new(BIN)
            .args(["--no-session", "ALPHA"])
            .args(&flags)
            .env("GREEG_STATS", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        if !flags.contains(&"discover") {
            input.write_all(b"alpha\n").unwrap();
        }
        drop(input);
        let out = child.wait_with_output().unwrap();
        assert!(out.stdout.is_empty());
        assert_eq!(
            out.status.code(),
            Some(if flags.contains(&"discover") { 2 } else { 1 })
        );
    }
}

#[cfg(unix)]
#[test]
fn output_failure_overrides_an_exact_match() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;

    let f = fixture();
    // Close the reader before launching, so the broken pipe is deterministic.
    let (writer, reader) = UnixStream::pair().unwrap();
    drop(reader);
    let output = Command::new(BIN)
        .args(["--no-session", "--no-index", "-l", "load_config", "."])
        .current_dir(&f.root)
        .env("GREEG_STATS", "0")
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!output.stderr.is_empty());
}

// ---------------------------------------------------------------------------
// index identity
// ---------------------------------------------------------------------------

/// Every file under `dir` outside `skip`, relative path → bytes.
fn files_under(dir: &Path, skip: &[&Path]) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if skip.iter().any(|s| p.starts_with(s)) {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push((
                    p.strip_prefix(dir).unwrap().to_path_buf(),
                    fs::read(&p).unwrap(),
                ));
            }
        }
    }
    out.sort();
    out
}

fn generation(layout: &Path) -> u64 {
    let m: serde_json::Value =
        serde_json::from_slice(&fs::read(layout.join("manifest")).unwrap()).unwrap();
    m["generation"].as_u64().unwrap()
}

/// Releases before 0.8 keep their index at the top of the directory. This
/// layout lives in its own `v<N>/` beside it and never touches those files, so
/// an old and a new binary on one repository neither rebuild nor delete each
/// other's index.
#[test]
fn an_older_layout_in_the_same_directory_is_left_alone() {
    let f = fixture();
    w(&f.index.join("manifest"), r#"{"format":5,"generation":3}"#);
    w(
        &f.index.join("files.3.bin"),
        "an older release's file table",
    );
    w(&f.index.join("grams.2.bin"), "an older generation");
    w(&f.index.join("delta/0001.bin"), "an older delta");
    let layout = greeg_index::format_dir(&f.index);
    let session = f.index.join("session");
    let before = files_under(&f.index, &[&layout, &session]);
    f.indexed();
    let built = generation(&layout);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let args = ["--fresh", "stat", "--budget", "0", "load_config"];
    let scanned = pairs(&f.out(&["--no-index", "--budget", "0", "load_config"]));
    assert_eq!(pairs(&f.out(&args)), scanned);
    assert_eq!(pairs(&f.out(&args)), scanned);
    assert_eq!(generation(&layout), built, "a query rebuilt the index");
    assert_eq!(files_under(&f.index, &[&layout, &session]), before);
    let owner = fs::read_to_string(layout.join("OWNER")).unwrap();
    assert_eq!(
        owner,
        format!(
            "greeg {} {}\n",
            env!("CARGO_PKG_VERSION"),
            greeg_index::FORMAT_VERSION
        )
    );
}

/// An index directory shared by two roots (`--index-dir`) answers only for
/// the root it was built for, even with freshness checks off.
#[test]
fn an_index_built_for_another_root_is_not_used() {
    let a = fixture();
    a.indexed();
    let b = fixture();
    w(&b.root.join("src/only_b.rs"), "fn bravo_only() {}\n");
    let o = Command::new(BIN)
        .args([
            "--no-session",
            "--fresh",
            "none",
            "--budget",
            "0",
            "bravo_only",
        ])
        .arg("--index-dir")
        .arg(&a.index)
        .current_dir(&b.root)
        .env("GREEG_STATS", "0")
        .output()
        .unwrap();
    assert_eq!(
        (
            o.status.code(),
            String::from_utf8_lossy(&o.stdout).into_owned()
        ),
        (Some(0), "src/only_b.rs:1:fn bravo_only() {}\n".into()),
        "{o:?}"
    );
}

/// `greeg index --check` from another root refuses the index instead of
/// publishing that root's changes into it.
#[test]
fn a_check_from_another_root_leaves_the_index_alone() {
    let a = fixture();
    a.indexed();
    let layout = greeg_index::format_dir(&a.index);
    let before = files_under(&layout, &[]);
    let b = fixture();
    w(&b.root.join("src/only_b.rs"), "fn bravo_only() {}\n");
    let o = Command::new(BIN)
        .args(["index", "--check", "--index-dir"])
        .arg(&a.index)
        .current_dir(&b.root)
        .env("GREEG_STATS", "0")
        .output()
        .unwrap();
    assert!(!o.status.success(), "{o:?}");
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("built for another root"),
        "{o:?}"
    );
    assert_eq!(files_under(&layout, &[]), before);
}

/// Write `names` (raw bytes, `/`-separated) below `root`, each holding `body`.
fn byte_tree(root: &Path, names: &[&[u8]], body: &str) {
    use std::os::unix::ffi::OsStrExt;
    for n in names {
        let p = root.join(std::ffi::OsStr::from_bytes(n));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }
}

/// The `path` of every `event` line of a JSON answer, sorted (`--sort`
/// does not apply to JSON).
fn json_paths(stdout: &[u8], event: &str) -> Vec<serde_json::Value> {
    let mut v: Vec<serde_json::Value> = String::from_utf8(stdout.to_vec())
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["type"] == event)
        .map(|v| v["data"]["path"].clone())
        .collect();
    v.sort_by_key(|p| p.to_string());
    v
}

/// Which backend answered, from the JSON footer's outcome.
fn source(stdout: &[u8]) -> String {
    let last: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(stdout).lines().last().unwrap()).unwrap();
    last["data"]["outcome"]["source"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn file_lists_name_every_path_byte_for_byte() {
    let f = empty_fixture();
    let mut names: Vec<&[u8]> = vec![
        b"a\\b.txt",
        b"-lead.txt",
        b"co:lon.txt",
        b"tab\tname.txt",
        b"new\nline.txt",
        b"sp ace.txt",
        b"back\\slash/in.txt",
        "caf\u{e9}.txt".as_bytes(),
    ];
    byte_tree(&f.root, &names, "pathneedle\n");
    names.sort();
    let listed: Vec<u8> = names.iter().flat_map(|n| [*n, b"\n"].concat()).collect();
    let parity: Vec<u8> = names
        .iter()
        .flat_map(|n| [*n, b":1:pathneedle\n"].concat())
        .collect();
    f.indexed();
    for backend in [&[][..], &["--no-index"][..]] {
        let run = |args: &[&str]| {
            let o = f.run(&[args, backend].concat());
            assert_eq!(o.status.code(), Some(0), "{args:?} {backend:?}: {o:?}");
            o.stdout
        };
        assert_eq!(
            run(&["-l", "--sort", "path", "pathneedle"]),
            listed,
            "{backend:?}"
        );
        assert_eq!(
            run(&["--budget", "0", "--sort", "path", "pathneedle"]),
            parity,
            "{backend:?}"
        );
        let json = run(&["--json", "pathneedle"]);
        let want: Vec<serde_json::Value> = names
            .iter()
            .map(|n| serde_json::json!({"text": std::str::from_utf8(n).unwrap()}))
            .collect();
        let mut want = want;
        want.sort_by_key(|p| p.to_string());
        assert_eq!(json_paths(&json, "begin"), want, "{backend:?}");
        assert_eq!(
            source(&json),
            if backend.is_empty() { "index" } else { "scan" }
        );
        // headers are read, not reopened: control bytes are escaped there
        let ranked = String::from_utf8(run(&["pathneedle"])).unwrap();
        assert!(ranked.contains("\ntab\\x09name.txt\n"), "{ranked}");
        assert!(ranked.contains("\nnew\\x0Aline.txt\n"), "{ranked}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_non_utf8_file_name_round_trips_through_the_index() {
    let f = empty_fixture();
    byte_tree(&f.root, &[b"caf\xff.txt", b"d\xfe/x.txt"], "needle\n");
    // enough other files that one edit is refreshed, not rebuilt
    for i in 0..100 {
        w(&f.root.join(format!("filler/f{i:03}.txt")), "filler\n");
    }
    f.indexed();
    for backend in [&[][..], &["--no-index"][..]] {
        let run = |args: &[&str]| f.run(&[args, backend].concat()).stdout;
        assert_eq!(
            run(&["--budget", "0", "--sort", "path", "needle"]),
            b"caf\xff.txt:1:needle\nd\xfe/x.txt:1:needle\n",
            "{backend:?}"
        );
        assert_eq!(
            run(&["-l", "--sort", "path", "needle"]),
            b"caf\xff.txt\nd\xfe/x.txt\n"
        );
        let json = run(&["--json", "needle"]);
        assert_eq!(
            json_paths(&json, "begin"),
            [
                serde_json::json!({"bytes": "Y2Fm/y50eHQ="}),
                serde_json::json!({"bytes": "ZP4veC50eHQ="})
            ]
        );
        assert_eq!(
            source(&json),
            if backend.is_empty() { "index" } else { "scan" }
        );
        let ranked = String::from_utf8(run(&["needle"])).unwrap();
        assert!(ranked.contains("caf\\xFF.txt\n"), "{ranked}");
    }
    // an edit is answered from disk, then from the delta it published
    std::thread::sleep(std::time::Duration::from_millis(300));
    byte_tree(&f.root, &[b"caf\xff.txt"], "needle\nfreshterm\n");
    for _ in 0..2 {
        assert_eq!(
            f.run(&["--budget", "0", "freshterm"]).stdout,
            b"caf\xff.txt:2:freshterm\n"
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let json = f.run(&["--json", "freshterm"]).stdout;
    assert_eq!(source(&json), "index");
    assert_eq!(
        json_paths(&json, "begin"),
        [serde_json::json!({"bytes": "Y2Fm/y50eHQ="})]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn skipped_entries_with_non_utf8_names_keep_coverage_known() {
    let f = empty_fixture();
    byte_tree(&f.root, &[b".h\xff.txt", b"seen.txt"], "needle\n");
    f.indexed();
    let args = ["--json", "-g", "*.txt", "needle"];
    let json = f.run(&args).stdout;
    assert_eq!(source(&json), "index", "the skipped record lists the name");
    assert_eq!(
        json_paths(&json, "begin"),
        json_paths(
            &f.run(&[&args[..], &["--no-index"]].concat()).stdout,
            "begin"
        )
    );
    assert_eq!(json_paths(&json, "begin").len(), 2);
}

#[test]
fn verb_json_paths_name_the_file_exactly() {
    let f = empty_fixture();
    let names: Vec<(&[u8], serde_json::Value)> = vec![
        (b"tab\tdefs.rs", serde_json::json!("tab\tdefs.rs")),
        // APFS refuses names that are not UTF-8
        #[cfg(target_os = "linux")]
        (b"caf\xff.rs", serde_json::json!({"bytes": "Y2Fm/y5ycw=="})),
    ];
    for (n, _) in &names {
        byte_tree(&f.root, &[n], "pub fn exact_path_fn() {}\n");
    }
    f.indexed();
    for backend in [&[][..], &["--no-index"][..]] {
        let o = f.run(&[&["def", "exact_path_fn", "--json"][..], backend].concat());
        assert_eq!(o.status.code(), Some(0), "{o:?}");
        let mut want: Vec<_> = names.iter().map(|(_, p)| p.clone()).collect();
        want.sort_by_key(|p| p.to_string());
        // `def` lines carry the path at the top level
        let mut got: Vec<serde_json::Value> = String::from_utf8(o.stdout)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "def")
            .map(|v| v["path"].clone())
            .collect();
        got.sort_by_key(|p| p.to_string());
        assert_eq!(got, want, "{backend:?}");
    }
}
