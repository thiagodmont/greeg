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

/// greeg, isolated from the developer's config file (no file can exist under
/// `/dev/null`) and `GREEG_BUDGET`.
fn greeg() -> Command {
    let mut c = Command::new(BIN);
    c.env("GREEG_CONFIG_DIR", "/dev/null/greeg-config")
        .env_remove("GREEG_BUDGET");
    c
}

impl Fixture {
    fn run(&self, args: &[&str]) -> Output {
        self.run_env(args, &[])
    }

    fn run_env(&self, args: &[&str], env: &[(&str, &std::ffi::OsStr)]) -> Output {
        greeg()
            .envs(env.iter().copied())
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
        let o = greeg()
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
    // a scan counts the skipped binary file, so its answer is not complete and
    // the footer names the demoted files; the index never holds that file
    let footer = f.err(&["--budget", "0", "--no-index", "handle_request"]);
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
            args.extend(["--json=legacy", query]);
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
            vec!["--json=legacy"],
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
            args.push("--json=legacy");
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
    let module = f.run(&["def", "módulo", "--matching", "exact", "--json=legacy"]);
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
    let mut c = greeg()
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
fn stdin_with_utf8_bom_matches_anchored_first_line() {
    use std::io::Write;
    use std::process::Stdio;
    let mut c = greeg()
        .args(["--no-session", "^foo"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GREEG_STATS", "0")
        .spawn()
        .unwrap();
    c.stdin
        .take()
        .unwrap()
        .write_all(b"\xEF\xBB\xBFfoo bar\nfoo baz\n")
        .unwrap();
    let o = c.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("foo bar"), "{out}");
    assert!(out.contains("foo baz"), "{out}");
    assert_eq!(o.status.code(), Some(0));
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
        let mut child = greeg()
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
    let output = greeg()
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
/// build lives in its own `v<N>-<key>/` beside it and never touches those
/// files, so an old and a new binary on one repository neither rebuild nor
/// delete each other's index.
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
    let o = greeg()
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
    let o = greeg()
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
        let json = run(&["--json=legacy", "pathneedle"]);
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
        let json = run(&["--json=legacy", "needle"]);
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
    let json = f.run(&["--json=legacy", "freshterm"]).stdout;
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
    let args = ["--json=legacy", "-g", "*.txt", "needle"];
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
        let o = f.run(&[&["def", "exact_path_fn", "--json=legacy"][..], backend].concat());
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

/// A build killed at any point never changes an answer: the next query
/// answers as a scan does, including files added since, and the next build
/// publishes.
#[test]
fn a_build_killed_partway_never_changes_an_answer() {
    let f = empty_fixture();
    for i in 0..3000 {
        w(
            &f.root.join(format!("d{:02}/f{i:04}.rs", i % 40)),
            &format!(
                "pub fn f{i}() -> u32 {{\n    {i}\n}}\n// needle {}\n",
                i % 7
            ),
        );
    }
    let answers = |f: &Fixture| {
        let scan = f.out(&["-l", "--sort", "path", "needle 3", "--no-index"]);
        let indexed = f.out(&["-l", "--sort", "path", "needle 3"]);
        (scan, indexed)
    };
    for (round, delay_ms) in [0u64, 2, 5, 10, 20, 40, 80, 160].into_iter().enumerate() {
        w(&f.root.join(format!("new/n{round}.rs")), "// needle 3\n");
        // past the 100 ms in which a verified index is trusted unchecked
        std::thread::sleep(std::time::Duration::from_millis(150));
        let mut build = greeg()
            .args(["index", "--quiet", "--no-session", "--index-dir"])
            .arg(&f.index)
            .current_dir(&f.root)
            .env("GREEG_STATS", "0")
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        let _ = build.kill();
        build.wait().unwrap();
        let (scan, indexed) = answers(&f);
        assert!(scan.contains(&format!("new/n{round}.rs")), "{scan}");
        assert_eq!(indexed, scan, "killed after {delay_ms} ms");
    }
    f.indexed();
    let (scan, indexed) = answers(&f);
    assert_eq!(indexed, scan, "after a full build");
    let check = f.run(&["index", "--check"]);
    let text = String::from_utf8_lossy(&check.stdout).into_owned()
        + &String::from_utf8_lossy(&check.stderr);
    assert!(text.contains("rebuild none"), "{text}");
}

fn json_rg(f: &Fixture, args: &[&str]) -> (Option<i32>, String) {
    let mut a = vec!["--json=rg", "--no-index"];
    a.extend_from_slice(args);
    let o = f.run(&a);
    (
        o.status.code(),
        String::from_utf8_lossy(&o.stdout).into_owned(),
    )
}

/// The summary without its elapsed times, which no two runs share.
fn without_elapsed(line: &str) -> String {
    let mut v: serde_json::Value = serde_json::from_str(line).unwrap();
    let d = &mut v["data"];
    d.as_object_mut().unwrap().shift_remove("elapsed_total");
    d["stats"].as_object_mut().unwrap().shift_remove("elapsed");
    v.to_string()
}

/// `--json=rg` writes ripgrep's records and nothing else, in ripgrep's
/// coordinates: past a UTF-8 BOM, in UTF-16 decoded, with `{"bytes"}` for a
/// line that is not UTF-8. The expected records are ripgrep 15's output.
#[test]
fn json_rg_writes_ripgreps_records() {
    let f = empty_fixture();
    fs::write(f.root.join("a.txt"), "a needle\nb\nneedle needle\n").unwrap();
    fs::write(f.root.join("bad.txt"), b"inv\xffneedle\n").unwrap();
    fs::write(f.root.join("bom.txt"), b"\xef\xbb\xbfx\nneedle\n").unwrap();
    fs::write(
        f.root.join("u16.txt"),
        b"\xff\xfen\x00e\x00e\x00d\x00l\x00e\x00\n\x00",
    )
    .unwrap();
    let (code, out) = json_rg(
        &f,
        &[
            "-B", "1", "needle", "a.txt", "bad.txt", "bom.txt", "u16.txt",
        ],
    );
    assert_eq!(code, Some(0));
    let lines: Vec<&str> = out.lines().collect();
    let want = [
        r#"{"type":"begin","data":{"path":{"text":"a.txt"}}}"#,
        r#"{"type":"match","data":{"path":{"text":"a.txt"},"lines":{"text":"a needle\n"},"line_number":1,"absolute_offset":0,"submatches":[{"match":{"text":"needle"},"start":2,"end":8}]}}"#,
        r#"{"type":"context","data":{"path":{"text":"a.txt"},"lines":{"text":"b\n"},"line_number":2,"absolute_offset":9,"submatches":[]}}"#,
        r#"{"type":"match","data":{"path":{"text":"a.txt"},"lines":{"text":"needle needle\n"},"line_number":3,"absolute_offset":11,"submatches":[{"match":{"text":"needle"},"start":0,"end":6},{"match":{"text":"needle"},"start":7,"end":13}]}}"#,
        r#"{"type":"end","data":{"path":{"text":"a.txt"},"binary_offset":null,"stats":{"elapsed":{"secs":0,"nanos":0,"human":"0.000000s"},"searches":1,"searches_with_match":1,"bytes_searched":25,"bytes_printed":584,"matched_lines":2,"matches":3}}}"#,
        r#"{"type":"begin","data":{"path":{"text":"bad.txt"}}}"#,
        r#"{"type":"match","data":{"path":{"text":"bad.txt"},"lines":{"bytes":"aW52/25lZWRsZQo="},"line_number":1,"absolute_offset":0,"submatches":[{"match":{"text":"needle"},"start":4,"end":10}]}}"#,
        r#"{"type":"end","data":{"path":{"text":"bad.txt"},"binary_offset":null,"stats":{"elapsed":{"secs":0,"nanos":0,"human":"0.000000s"},"searches":1,"searches_with_match":1,"bytes_searched":11,"bytes_printed":239,"matched_lines":1,"matches":1}}}"#,
        r#"{"type":"begin","data":{"path":{"text":"bom.txt"}}}"#,
        r#"{"type":"context","data":{"path":{"text":"bom.txt"},"lines":{"text":"x\n"},"line_number":1,"absolute_offset":0,"submatches":[]}}"#,
        r#"{"type":"match","data":{"path":{"text":"bom.txt"},"lines":{"text":"needle\n"},"line_number":2,"absolute_offset":2,"submatches":[{"match":{"text":"needle"},"start":0,"end":6}]}}"#,
        r#"{"type":"end","data":{"path":{"text":"bom.txt"},"binary_offset":null,"stats":{"elapsed":{"secs":0,"nanos":0,"human":"0.000000s"},"searches":1,"searches_with_match":1,"bytes_searched":9,"bytes_printed":358,"matched_lines":1,"matches":1}}}"#,
        r#"{"type":"begin","data":{"path":{"text":"u16.txt"}}}"#,
        r#"{"type":"match","data":{"path":{"text":"u16.txt"},"lines":{"text":"needle\n"},"line_number":1,"absolute_offset":0,"submatches":[{"match":{"text":"needle"},"start":0,"end":6}]}}"#,
        r#"{"type":"end","data":{"path":{"text":"u16.txt"},"binary_offset":null,"stats":{"elapsed":{"secs":0,"nanos":0,"human":"0.000000s"},"searches":1,"searches_with_match":1,"bytes_searched":7,"bytes_printed":229,"matched_lines":1,"matches":1}}}"#,
    ];
    assert_eq!(lines[..want.len()], want[..], "{out}");
    assert_eq!(lines.len(), want.len() + 1, "{out}");
    assert_eq!(
        without_elapsed(lines[want.len()]),
        r#"{"data":{"stats":{"bytes_printed":1410,"bytes_searched":52,"matched_lines":5,"matches":6,"searches":4,"searches_with_match":4}},"type":"summary"}"#
    );
    assert!(
        lines[want.len()].starts_with(r#"{"data":{"elapsed_total":{"human":"#),
        "{out}"
    );
}

/// `--json=rg` keeps ripgrep's semantics: every match whatever the budget,
/// `-l` and `-c` as text, exit 1 without a match; `--budget`, commands and
/// `-U`, whose records it cannot match, refuse it.
#[test]
fn json_rg_keeps_ripgreps_semantics() {
    let f = empty_fixture();
    let body: String = (0..400).map(|i| format!("needle {i}\n")).collect();
    fs::write(f.root.join("a.txt"), body).unwrap();
    let (code, out) = json_rg(&f, &["needle"]);
    assert_eq!(code, Some(0));
    assert_eq!(out.matches(r#""type":"match""#).count(), 400);
    assert!(
        !out.contains(r#""type":"footer""#) && !out.contains(r#""kind""#),
        "{out}"
    );
    assert_eq!(json_rg(&f, &["-l", "needle"]), (Some(0), "a.txt\n".into()));
    assert_eq!(
        json_rg(&f, &["-c", "needle"]),
        (Some(0), "a.txt:400\n".into())
    );
    let (code, out) = json_rg(&f, &["zzz"]);
    assert_eq!(code, Some(1));
    assert!(
        out.starts_with(r#"{"data":"#) && out.lines().count() == 1,
        "{out}"
    );
    for args in [
        &["--budget", "10", "needle"][..],
        &["def", "needle"][..],
        &["-U", "needle"][..],
    ] {
        let (code, out) = json_rg(&f, args);
        assert_eq!((code, out.as_str()), (Some(2), ""), "{args:?}");
    }
}

/// `--json=rg` keeps every occurrence on a line, and its `bytes_searched`
/// leaves out a file skipped as binary, as ripgrep does, in both backends.
#[test]
fn json_rg_counts_every_occurrence_and_only_searched_bytes() {
    let f = empty_fixture();
    let line = ["needle"; 10].join(" ") + "\n";
    fs::write(f.root.join("a.txt"), &line).unwrap();
    let mut near = b"needle\n".to_vec();
    near.extend([b'x'; 9000]);
    near.extend(b"\n\0\n");
    fs::write(f.root.join("near.txt"), near).unwrap();
    f.indexed();
    for backend in [&["--no-index"][..], &[][..]] {
        let mut args = vec!["--json=rg", "needle"];
        args.extend_from_slice(backend);
        let out = f.out(&args);
        assert_eq!(
            out.matches(r#"{"match":{"text":"needle"}"#).count(),
            10,
            "{out}"
        );
        assert!(
            out.contains(
                r#""bytes_searched":70,"bytes_printed":719,"matched_lines":1,"matches":10}"#
            ),
            "{out}"
        );
        assert!(
            out.contains(r#""stats":{"bytes_printed":719,"bytes_searched":70,"#),
            "{out}"
        );
    }
}

/// A NUL past the first 64 KiB ends a file's search at the start of its
/// line: the matches before it are kept and the output says where it stopped,
/// in both backends, with `-U`, and on stdin. A NUL within 64 KiB skips the
/// file.
#[test]
fn a_late_nul_ends_the_search_and_says_where() {
    let f = empty_fixture();
    let fill = format!("{}\n", "x".repeat(79)).repeat(2600);
    let at = |t: &str| (7 + fill.len() + t.len()) as u64;
    let late = format!("needle\n{fill}\0\nneedle after\n");
    fs::write(f.root.join("far.txt"), &late).unwrap();
    fs::write(f.root.join("bom.txt"), format!("\u{feff}{late}")).unwrap();
    fs::write(
        f.root.join("same.txt"),
        format!("needle\n{fill}needle \0\nneedle after\n"),
    )
    .unwrap();
    fs::write(
        f.root.join("early.txt"),
        format!("needle\n{}\0\nneedle after\n", &fill[..20_000]),
    )
    .unwrap();
    let warn = |p: &str, off: u64| {
        format!(
            "{p}WARNING: stopped searching binary file after match (found \"\\0\" byte around offset {off})\n"
        )
    };
    let (far, same) = (at(""), at("needle "));
    f.indexed();
    for backend in [&["--no-index"][..], &[][..]] {
        let run = |args: &[&str]| {
            let mut a = args.to_vec();
            a.extend_from_slice(backend);
            let o = f.run(&a);
            assert_eq!(o.status.code(), Some(0), "{args:?} {o:?}");
            String::from_utf8(o.stdout).unwrap()
        };
        let text = format!(
            "bom.txt:1:needle\n{}far.txt:1:needle\n{}same.txt:1:needle\n{}",
            warn("bom.txt: ", far),
            warn("far.txt: ", far),
            warn("same.txt: ", same)
        );
        assert_eq!(run(&["--budget", "0", "needle"]), text);
        assert_eq!(run(&["--budget", "0", "-U", "needle"]), text);
        assert_eq!(run(&["-c", "needle"]), "bom.txt:1\nfar.txt:1\nsame.txt:1\n");
        assert_eq!(run(&["-l", "needle"]), "bom.txt\nfar.txt\nsame.txt\n");
        let footer = run(&["needle"]);
        assert!(
            footer.contains("skipped 1 binary, 3 binary tails"),
            "{footer}"
        );

        let ends = |dialect: &str| -> Vec<(String, u64, u64)> {
            json_lines(&run(&[dialect, "needle"]))
                .into_iter()
                .filter(|r| r["type"] == "end")
                .map(|r| {
                    let d = &r["data"];
                    (
                        d["path"]["text"].as_str().unwrap().to_string(),
                        d["binary_offset"].as_u64().unwrap(),
                        d["stats"]["bytes_searched"].as_u64().unwrap(),
                    )
                })
                .collect()
        };
        // searched up to the start of the NUL's line, which is where `far` is
        assert_eq!(
            ends("--json=rg"),
            [
                ("bom.txt".into(), far, far),
                ("far.txt".into(), far, far),
                ("same.txt".into(), same, far)
            ]
        );
        // legacy offsets and searched bytes count a UTF-8 BOM
        assert_eq!(
            ends("--json=legacy"),
            [
                ("bom.txt".into(), far + 3, far + 3),
                ("far.txt".into(), far, far),
                ("same.txt".into(), same, far)
            ]
        );
        let records = json_lines(&run(&["--json=legacy", "needle"]));
        let of = |t: &str| records.iter().find(|r| r["type"] == t).unwrap()["data"].clone();
        assert_eq!(of("summary")["stats"]["bytes_searched"], 3 * far + 3);
        assert_eq!(of("footer")["binary_tails"], 3);
        let legacy = [
            ("bom.txt".to_string(), far + 3),
            ("far.txt".into(), far),
            ("same.txt".into(), same),
        ];

        let native = json_lines(&run(&["--json=greeg", "needle"]));
        let begins: Vec<(String, u64)> = native
            .iter()
            .filter(|r| r["type"] == "begin")
            .map(|r| {
                let d = &r["data"];
                (
                    d["path"]["text"].as_str().unwrap().to_string(),
                    d["binary_offset"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(begins, legacy);
        let footer = &native.last().unwrap()["data"];
        assert_eq!(
            (&footer["skipped_binary"], &footer["binary_tails"]),
            (&serde_json::json!(1), &serde_json::json!(3))
        );
    }

    use std::io::Write;
    use std::process::Stdio;
    let mut c = greeg()
        .args(["--no-session", "needle"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GREEG_STATS", "0")
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(late.as_bytes()).unwrap();
    let o = c.wait_with_output().unwrap();
    assert_eq!(
        (o.status.code(), String::from_utf8(o.stdout).unwrap()),
        (Some(0), format!("needle\n{}", warn("", far)))
    );
}

/// A file without a late NUL has no `binary_offset`, in any dialect.
#[test]
fn a_text_file_has_no_binary_offset() {
    let f = empty_fixture();
    fs::write(f.root.join("a.txt"), "needle\n").unwrap();
    for dialect in ["--json=rg", "--json=legacy"] {
        let out = f.out(&[dialect, "--no-index", "needle"]);
        assert!(out.contains(r#""binary_offset":null"#), "{out}");
        assert!(!out.contains("binary_tails"), "{out}");
    }
    let out = f.out(&["--json=greeg", "--no-index", "needle"]);
    assert!(!out.contains("binary_offset"), "{out}");
    assert!(!out.contains("binary_tails"), "{out}");
}

/// The first 64 KiB start after a UTF-8 BOM, as in ripgrep, which strips the
/// BOM before its first read.
#[test]
fn a_bom_does_not_count_toward_the_first_64_kib() {
    let f = empty_fixture();
    for (name, nul) in [("skipped.txt", 65_535), ("tail.txt", 65_536)] {
        let mut body =
            format!("needle\n{}", format!("{}\n", "x".repeat(79)).repeat(900)).into_bytes();
        body[nul] = 0;
        let mut file = b"\xef\xbb\xbf".to_vec();
        file.extend(body);
        fs::write(f.root.join(name), file).unwrap();
    }
    f.indexed();
    for backend in ["--no-index", "--fresh=stat"] {
        assert_eq!(f.out(&["-l", "needle", backend]), "tail.txt\n");
    }
}

/// `impact` cuts its callers to the budget: the outcome counts them, says the
/// answer is incomplete, and text says how many were left out, in every
/// format and both backends.
#[test]
fn impact_counts_the_callers_it_leaves_out() {
    let f = empty_fixture();
    let mut src = String::from("pub fn target() {}\n");
    for i in 0..5 {
        src.push_str(&format!("pub fn caller{i}() {{\n    target();\n}}\n"));
    }
    w(&f.root.join("src/lib.rs"), &src);
    f.indexed();
    for backend in [&["--no-index"][..], &["--fresh", "stat"][..]] {
        let run = |extra: &[&str]| {
            let mut a = vec!["impact", "target", "--budget", "300"];
            a.extend_from_slice(extra);
            a.extend_from_slice(backend);
            f.out(&a)
        };
        let native = json_lines(&run(&["--json=greeg"]));
        assert_eq!(native[1]["data"]["callers"].as_array().unwrap().len(), 3);
        let footer = &native.last().unwrap()["data"];
        assert_eq!(footer["files"], 1, "{footer}");
        assert_eq!(footer["callers_total"], 5, "{footer}");
        let oc = &footer["outcome"];
        assert_eq!(
            (&oc["total"], &oc["shown"], &oc["complete"]),
            (
                &serde_json::json!(6),
                &serde_json::json!(4),
                &serde_json::json!(false)
            ),
            "{backend:?} {oc}"
        );

        let legacy = json_lines(&run(&["--json=legacy"]));
        let footer = &legacy.last().unwrap()["data"];
        assert_eq!(footer["callers_total"], 5, "{footer}");
        assert_eq!(footer["outcome"]["complete"], false, "{footer}");

        let text = run(&[]);
        assert!(text.contains("\n  +2 more\n"), "{text}");
        assert!(text.ends_with("\n3/5 callers · raise --budget\n"), "{text}");
    }
}

/// `outline`, `show` and `map` report the freshness check their index
/// answer ran, as every other verb does.
#[test]
fn file_verbs_report_their_freshness_check() {
    let f = empty_fixture();
    w(&f.root.join("src/lib.rs"), "pub fn alpha() {\n    1;\n}\n");
    f.indexed();
    for args in [
        &["outline", "src/lib.rs"][..],
        &["show", "src/lib.rs:2"][..],
        &["map", "src"][..],
    ] {
        let mut a = args.to_vec();
        a.extend(["--json=greeg", "--fresh", "stat"]);
        let oc = &json_lines(&f.out(&a)).last().unwrap()["data"]["outcome"].clone();
        assert_eq!(
            (&oc["source"], &oc["fresh"]),
            (&serde_json::json!("index"), &serde_json::json!("stat")),
            "{args:?} {oc}"
        );
    }
    let mut a = vec!["outline", "src/lib.rs", "--json=greeg", "--no-index"];
    let oc = &json_lines(&f.out(&a)).last().unwrap()["data"]["outcome"].clone();
    assert_eq!(oc["fresh"], "", "{oc}");
    a[0] = "show";
    a[1] = "src/lib.rs:2";
    let oc = &json_lines(&f.out(&a)).last().unwrap()["data"]["outcome"].clone();
    assert_eq!(oc["fresh"], "", "{oc}");
}

/// `show` with several locations reports one source for all of them, in any
/// order: `index` only when every location came from the index.
#[test]
fn show_reports_the_source_of_every_location() {
    let f = empty_fixture();
    w(&f.root.join("src/lib.rs"), "pub fn alpha() {\n    1;\n}\n");
    w(&f.root.join("notes.md"), "one\ntwo\n");
    f.indexed();
    let outcome = |locs: &[&str]| {
        let mut a = vec!["show"];
        a.extend_from_slice(locs);
        a.extend(["--json=greeg", "--fresh", "stat"]);
        let oc = json_lines(&f.out(&a)).last().unwrap()["data"]["outcome"].clone();
        (oc["source"].clone(), oc["fresh"].clone())
    };
    let text = (serde_json::json!("text"), serde_json::json!(""));
    assert_eq!(outcome(&["src/lib.rs:2", "notes.md:1"]), text);
    assert_eq!(outcome(&["notes.md:1", "src/lib.rs:2"]), text);
    assert_eq!(
        outcome(&["src/lib.rs:2", "src/lib.rs:1"]),
        (serde_json::json!("index"), serde_json::json!("stat"))
    );
}

/// A tree where every command can be cut by a small budget.
fn outcome_fixture() -> Fixture {
    let f = empty_fixture();
    let mut lib = String::from(
        "pub trait Shape {\n    fn area(&self) -> u32;\n}\npub fn target() -> u32 {\n    1\n}\n",
    );
    for i in 0..8 {
        lib.push_str(&format!(
            "pub fn caller{i}() -> u32 {{\n    target()\n}}\npub struct S{i};\nimpl Shape for S{i} {{\n    fn area(&self) -> u32 {{\n        target()\n    }}\n}}\n"
        ));
    }
    w(&f.root.join("src/lib.rs"), &lib);
    w(
        &f.root.join("src/user.rs"),
        "use crate::target;\npub fn user() -> u32 {\n    target()\n}\n",
    );
    for i in 0..8 {
        w(
            &f.root.join(format!("src/a{i}.rs")),
            "pub fn target() -> u32 {\n    2\n}\n",
        );
    }
    f.indexed();
    f
}

/// Every verb's text fits its budget: `--budget 1` gives the floor (header,
/// counts, outcome), and any budget at or above the floor's estimated tokens
/// bounds the answer; below it, the answer is the floor.
#[test]
fn verb_text_fits_its_budget() {
    let f = outcome_fixture();
    let est = |b: &[u8]| greeg_query::tokens::estimate(b);
    let queries: [&[&str]; 8] = [
        &["def", "target"],
        &["refs", "target"],
        &["callers", "target"],
        &["impls", "Shape"],
        &["impact", "target"],
        &["outline", "src/lib.rs"],
        &["show", "src/lib.rs:12", "src/lib.rs:20"],
        &["map", "src"],
    ];
    for backend in [&["--fresh", "stat"][..], &["--no-index"][..]] {
        for q in queries {
            if backend == ["--no-index"] && q[0] == "map" {
                continue;
            }
            let run = |budget: usize| {
                let mut a: Vec<String> = q.iter().map(|s| s.to_string()).collect();
                a.extend(backend.iter().map(|s| s.to_string()));
                a.extend(["--budget".to_string(), budget.to_string()]);
                let a: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
                f.run(&a).stdout
            };
            let floor = run(1);
            for budget in [20, 40, 60, 80, 120, 160, 240, 320, 480, 640, 1000] {
                let out = run(budget);
                if budget < est(&floor) {
                    assert_eq!(
                        String::from_utf8_lossy(&out),
                        String::from_utf8_lossy(&floor),
                        "{q:?} {backend:?} budget {budget} below the floor"
                    );
                } else {
                    assert!(
                        est(&out) <= budget,
                        "{q:?} {backend:?} budget {budget}: ~{} tokens\n{}",
                        est(&out),
                        String::from_utf8_lossy(&out)
                    );
                }
            }
        }
    }
}

/// A `def` that finds only near names fits their list to the budget too.
#[test]
fn def_suggestions_fit_the_budget() {
    let f = empty_fixture();
    let mut src = String::new();
    for i in 0..30u32 {
        let name: String = "targetname"
            .chars()
            .enumerate()
            .map(|(k, c)| {
                if i >> k & 1 == 1 {
                    c.to_ascii_uppercase()
                } else {
                    c
                }
            })
            .collect();
        src.push_str(&format!("pub fn {name}() -> u32 {{\n    {i}\n}}\n"));
    }
    w(&f.root.join("src/lib.rs"), &src);
    f.indexed();
    let est = |b: &[u8]| greeg_query::tokens::estimate(b);
    let run = |budget: &str| {
        f.run(&[
            "def",
            "TARGETNAME",
            "--def-kind",
            "class",
            "--budget",
            budget,
        ])
        .stdout
    };
    // `--budget 0` turns the ladder off; a large budget keeps every suggestion
    let all = run("100000");
    assert!(
        String::from_utf8_lossy(&all).contains("closest names: "),
        "{}",
        String::from_utf8_lossy(&all)
    );
    let floor = run("1");
    assert!(est(&floor) < est(&all));
    for budget in [20, 40, 60, 80] {
        let out = run(&budget.to_string());
        if budget >= est(&floor) {
            assert!(est(&out) <= budget, "{}", String::from_utf8_lossy(&out));
        }
    }
}

/// `--budget` wins, then `GREEG_BUDGET`, then the level `greeg budget` saved,
/// then 2000. A saved level never makes `--json=rg` refuse to run.
#[test]
fn budget_levels_set_the_default() {
    let f = empty_fixture();
    for i in 0..200 {
        w(
            &f.root.join(format!("src/m{i}.rs")),
            &format!("pub fn caller_{i}() -> u32 {{\n    target_value({i})\n}}\n"),
        );
    }
    f.indexed();
    let config = f.base.join("config");
    let dir = config.as_os_str();
    let run = |args: &[&str], extra: &[(&str, &std::ffi::OsStr)]| {
        let mut env = vec![("GREEG_CONFIG_DIR", dir)];
        env.extend_from_slice(extra);
        f.run_env(args, &env)
    };
    let text = |o: Output| String::from_utf8_lossy(&o.stdout).into_owned();
    let search = |budget: Option<&str>, extra: &[(&str, &std::ffi::OsStr)]| {
        let mut a = vec!["refs", "target_value"];
        if let Some(b) = budget {
            a.extend(["--budget", b]);
        }
        text(run(&a, extra))
    };
    let at = |n: &str| search(Some(n), &[]);
    assert_ne!(at("1000"), at("2000"), "the fixture must be cut at 1000");

    assert!(text(run(&["budget"], &[])).starts_with("budget 2000 (medium) · default"));
    assert_eq!(search(None, &[]), at("2000"));

    let o = run(&["budget", "low"], &[]);
    assert_eq!(o.status.code(), Some(0));
    assert!(
        fs::read_to_string(config.join("config.toml"))
            .unwrap()
            .contains("budget = 1000")
    );
    assert!(text(run(&["budget"], &[])).starts_with("budget 1000 (low) · "));
    assert_eq!(search(None, &[]), at("1000"));
    assert_eq!(
        search(Some("medium"), &[]),
        at("2000"),
        "--budget overrides it"
    );
    let high = std::ffi::OsStr::new("high");
    assert_eq!(
        search(None, &[("GREEG_BUDGET", high)]),
        at("5000"),
        "GREEG_BUDGET overrides it"
    );
    let rg = run(&["target_value", "--json=rg"], &[]);
    assert_eq!(
        rg.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&rg.stderr)
    );

    let none = text(run(&["budget", "none"], &[]));
    assert!(none.contains("every match"), "{none}");
    assert_eq!(search(None, &[]), at("0"));

    let bad = run(&["target_value", "--budget", "lots"], &[]);
    assert_eq!(bad.status.code(), Some(2));
    let lots = std::ffi::OsStr::new("lots");
    let o = run(&["budget"], &[("GREEG_BUDGET", lots)]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("ignoring GREEG_BUDGET"));

    // a pattern spelled like the command is a search: no hits, exit 1
    let o = run(&["-e", "budget", "--budget", "0"], &[]);
    assert_eq!(
        o.status.code(),
        Some(1),
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(o.stdout.is_empty());

    // `--budget` is this run's budget, also for `greeg budget`, which saves
    // only its argument
    assert!(
        text(run(&["budget", "--budget", "low"], &[])).starts_with("budget 1000 (low) · --budget")
    );
    assert_eq!(
        run(&["budget", "high", "--budget", "low"], &[])
            .status
            .code(),
        Some(2)
    );
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let o = run(
            &["budget"],
            &[("GREEG_BUDGET", std::ffi::OsStr::from_bytes(b"\xff"))],
        );
        assert!(String::from_utf8_lossy(&o.stderr).contains("not valid UTF-8"));
    }
    // a level written by hand as a TOML string
    fs::write(config.join("config.toml"), "budget = 'high'\n").unwrap();
    assert_eq!(search(None, &[]), at("5000"));
}

/// The last line of a text answer that states its outcome, before `next:`.
fn outcome_line(text: &str) -> Option<&str> {
    text.lines()
        .rev()
        .find(|l| !l.is_empty() && !l.starts_with("next:"))
        .filter(|l| {
            l.contains("raise --budget")
                || l.contains("matched ")
                || l.contains("index not checked for changes")
                || l.contains(" hits")
        })
}

/// Every text answer states its outcome as `--json=greeg` does: the same
/// exit status; cut results counted shown/total with how to get the rest;
/// a relaxed match named; an index answer that skipped the freshness check
/// said so. A complete, exact, checked verb answer adds no outcome line.
#[test]
fn text_states_its_outcome_as_json_does() {
    let f = outcome_fixture();
    let cases: &[&[&str]] = &[
        &["target"],
        &["target", "--budget", "100"],
        &["targte"],
        &["zzz_none"],
        &["def", "target"],
        &["def", "target", "--budget", "100"],
        &["def", "targte"],
        &["def", "zzz_none"],
        &["refs", "target"],
        &["refs", "target", "--budget", "150"],
        &["refs", "target", "--budget", "0"],
        &["callers", "target"],
        &["callers", "target", "--budget", "50"],
        &["impls", "Shape"],
        &["impls", "Shape", "--budget", "50"],
        &["impact", "target"],
        &["impact", "target", "--budget", "300"],
        &["outline", "src/lib.rs"],
        &["outline", "src/lib.rs", "--budget", "50"],
        &["show", "src/lib.rs:5"],
        &["map", "src"],
        &["map", "src", "--budget", "50"],
    ];
    for backend in [
        &["--no-index"][..],
        &["--fresh", "stat"][..],
        &["--fresh", "none"][..],
    ] {
        for case in cases {
            if case[0] == "map" && backend[0] == "--no-index" {
                continue;
            }
            let mut args = case.to_vec();
            args.extend_from_slice(backend);
            let t = f.run(&args);
            let text = String::from_utf8_lossy(&t.stdout).into_owned()
                + &String::from_utf8_lossy(&t.stderr);
            args.push("--json=greeg");
            let j = f.run(&args);
            let records = json_lines(&String::from_utf8_lossy(&j.stdout));
            let footer = &records.last().unwrap()["data"];
            let oc = &footer["outcome"];
            let ctx = format!("{args:?}\n{text}\n{oc}");
            assert_eq!(t.status.code(), j.status.code(), "{ctx}");
            let line = outcome_line(&text);
            let verb = match case[0] {
                v @ ("def" | "refs" | "callers" | "impls" | "impact" | "outline" | "show"
                | "map") => v,
                _ => "search",
            };

            let unchecked = backend.contains(&"none") && oc["source"] != "scan";
            assert_eq!(
                text.contains("index not checked for changes"),
                unchecked,
                "{ctx}"
            );
            if unchecked {
                assert!(
                    line.is_some_and(|l| l.contains("index not checked")),
                    "{ctx}"
                );
            }

            let found_nothing = text.contains("no definition found")
                || text.contains("no hits")
                || text.contains("no references");
            let relaxed = t.status.code() == Some(1) && !found_nothing;
            if relaxed {
                assert!(line.is_some_and(|l| l.contains("matched ")), "{ctx}");
            }

            let (shown, total) = (oc["shown"].as_u64().unwrap(), oc["total"].as_u64().unwrap());
            // a small budget must cut, or the checks below would pass vacuously
            if args.windows(2).any(|a| a[0] == "--budget" && a[1] != "0") {
                assert!(
                    text.lines()
                        .any(|l| l.trim_start().starts_with('+') && l.contains(" more"))
                        || line.is_some_and(|l| l.contains('/')),
                    "a small budget must cut: {ctx}"
                );
            }
            let cut_marker = text
                .lines()
                .any(|l| l.trim_start().starts_with('+') && l.contains(" more"));
            match verb {
                // search JSON is shaped by the budget as text is
                "search" if shown < total => {
                    let l = line.unwrap_or_else(|| panic!("no outcome line: {ctx}"));
                    assert!(l.contains(&format!("{shown}/{total} ")), "{ctx}");
                }
                // each format counts the rows it fits; `refs` text folds
                // imports into one line
                "def" | "callers" | "outline" | "impls" | "refs" if cut_marker => {
                    let l = line.unwrap_or_else(|| panic!("no outcome line: {ctx}"));
                    let unit = match verb {
                        "def" => "definitions",
                        "callers" => "callers",
                        "outline" => "symbols",
                        "impls" => "implementations",
                        _ => "hits",
                    };
                    assert!(l.contains(&format!("/{total} {unit}")), "{ctx}");
                    assert!(l.contains("raise --budget"), "{ctx}");
                }
                "map" if cut_marker => {
                    let l = line.unwrap_or_else(|| panic!("no outcome line: {ctx}"));
                    assert!(
                        l.contains(&format!("/{} files", footer["files_total"])),
                        "{ctx}"
                    );
                    assert!(
                        l.contains("raise --budget or narrow the directory"),
                        "{ctx}"
                    );
                }
                "impact" if cut_marker => {
                    let l = line.unwrap_or_else(|| panic!("no outcome line: {ctx}"));
                    assert!(
                        l.contains(&format!("/{} callers", footer["callers_total"])),
                        "{ctx}"
                    );
                    assert!(l.contains("raise --budget"), "{ctx}");
                }
                _ if verb != "search" && !relaxed && !unchecked => {
                    assert!(
                        line.is_none(),
                        "complete answers add no outcome line: {ctx}"
                    );
                    assert!(!cut_marker, "{ctx}");
                }
                _ => {}
            }
            if cut_marker && verb != "search" {
                assert!(line.is_some_and(|l| l.contains("raise --budget")), "{ctx}");
            }
        }
    }
}

/// `impact` grades each referring file by its evidence: a call, type use or
/// import is `likely affected` only when the file imports a file defining
/// the name, directly or through one module. A use without that link, or a
/// member or bare-name use, is `possible`. A scan has no import graph, so
/// nothing is likely there, and the answer says so.
#[test]
fn impact_grades_files_by_evidence() {
    let f = empty_fixture();
    for (path, body) in [
        (
            "src/lib.rs",
            "pub mod defs;\npub mod reexport;\npub mod direct;\npub mod via;\npub mod unlinked;\npub mod member;\n",
        ),
        ("src/defs.rs", "pub fn target() -> u32 {\n    1\n}\n"),
        ("src/reexport.rs", "pub use crate::defs::target;\n"),
        (
            "src/direct.rs",
            "use crate::defs::target;\npub fn a() -> u32 {\n    target()\n}\n",
        ),
        (
            "src/via.rs",
            "use crate::reexport::target;\npub fn b() -> u32 {\n    target()\n}\n",
        ),
        (
            "src/unlinked.rs",
            "pub fn c() -> u32 {\n    other::target()\n}\n",
        ),
        (
            "src/member.rs",
            "pub fn d(x: &crate::X) -> u32 {\n    x.target\n}\n",
        ),
        (
            "tests/t.rs",
            "#[test]\nfn t() {\n    fixture::defs::target();\n}\n",
        ),
    ] {
        w(&f.root.join(path), body);
    }
    f.indexed();
    let groups = |backend: &[&str]| {
        let mut a = vec!["impact", "target", "--json=greeg"];
        a.extend_from_slice(backend);
        let records = json_lines(&f.out(&a));
        assert_eq!(records[0]["data"]["schema"], 2);
        let d = records[1]["data"].clone();
        let paths = |k: &str| -> Vec<String> {
            let mut v: Vec<String> = d[k]
                .as_array()
                .unwrap_or_else(|| panic!("{k}: {d}"))
                .iter()
                .map(|r| r["path"]["text"].as_str().unwrap().to_string())
                .collect();
            v.sort();
            v
        };
        (
            paths("likely"),
            paths("possible"),
            paths("review"),
            d["import_graph"].clone(),
        )
    };
    let (likely, possible, review, graph) = groups(&["--fresh", "stat"]);
    assert_eq!(
        (likely, possible, review, graph),
        (
            vec![
                "src/direct.rs".to_string(),
                "src/reexport.rs".into(),
                "src/via.rs".into()
            ],
            vec!["src/member.rs".to_string(), "src/unlinked.rs".into()],
            vec!["src/defs.rs".to_string(), "tests/t.rs".into()],
            serde_json::json!(true)
        )
    );
    let (likely, possible, _, graph) = groups(&["--no-index"]);
    assert!(likely.is_empty(), "{likely:?}");
    assert_eq!(possible.len(), 5, "{possible:?}");
    assert_eq!(graph, false);

    // legacy keeps its field names, holding the new groups
    let legacy = json_lines(&f.out(&["impact", "target", "--json=legacy", "--fresh", "stat"]));
    let d = &legacy[0]["data"];
    assert_eq!(d["will_break"].as_array().unwrap().len(), 3, "{d}");
    assert_eq!(d["may_break"].as_array().unwrap().len(), 2, "{d}");

    let text = f.out(&["impact", "target", "--fresh", "stat"]);
    assert!(text.contains("\nLIKELY AFFECTED (3 files"), "{text}");
    assert!(text.contains("\nPOSSIBLE (2 files"), "{text}");
    assert!(text.contains("\ncallers by name ("), "{text}");
    assert!(!text.contains("BREAK"), "{text}");
    let scan = f.out(&["impact", "target", "--no-index"]);
    assert!(
        scan.lines().next().unwrap().contains("no import graph"),
        "{scan}"
    );
    assert!(!scan.contains("LIKELY AFFECTED"), "{scan}");
}

/// A scan lists the implementations the index lists, with the same
/// confidence: both read supertypes from the parse, not from the text.
#[test]
fn impls_agree_between_scan_and_index() {
    let f = empty_fixture();
    w(
        &f.root.join("src/lib.rs"),
        "pub trait Shape {\n    fn area(&self) -> u32;\n}\npub struct S0;\nimpl Shape for S0 {\n    fn area(&self) -> u32 {\n        1\n    }\n}\npub struct S1;\nimpl crate::Shape for S1 {\n    fn area(&self) -> u32 {\n        1\n    }\n}\npub trait Round: Shape {}\npub fn take(_x: &dyn Shape) {}\n",
    );
    w(
        &f.root.join("src/m.py"),
        "class Base:\n    pass\n\n\nclass Kid(Base):\n    pass\n",
    );
    w(
        &f.root.join("src/m.ts"),
        "export interface Base {}\nexport class K implements Base {}\nexport function g(b: Base) {}\n",
    );
    // the index extracts no symbols from a minified file
    w(&f.root.join("src/m.min.js"), "class M extends Base {}\n");
    f.indexed();
    let found = |name: &str, backend: &[&str]| {
        let mut a = vec!["impls", name, "--json=greeg"];
        a.extend_from_slice(backend);
        let mut v: Vec<String> = json_lines(&f.out(&a))
            .iter()
            .filter(|r| r["type"] == "impl")
            .map(|r| {
                let d = &r["data"];
                format!(
                    "{}:{} {} {} {}",
                    d["path"]["text"].as_str().unwrap(),
                    d["line"],
                    d["kind"].as_str().unwrap(),
                    d["name"].as_str().unwrap(),
                    d["confidence"].as_str().unwrap()
                )
            })
            .collect();
        v.sort();
        v
    };
    for (name, want) in [
        (
            "Shape",
            vec![
                "src/lib.rs:11 impl S1 high",
                "src/lib.rs:16 trait Round high",
                "src/lib.rs:5 impl S0 high",
            ],
        ),
        (
            "Base",
            vec![
                "src/m.min.js:1 class M low",
                "src/m.py:5 class Kid high",
                "src/m.ts:2 class K high",
            ],
        ),
    ] {
        assert_eq!(found(name, &["--fresh", "stat"]), want, "index {name}");
        assert_eq!(found(name, &["--no-index"]), want, "scan {name}");
    }
}

/// Case folding finds a module that defines the name, so its importers are
/// linked with `-i` as they are without it.
#[test]
fn impact_folds_case_for_module_definitions() {
    let f = empty_fixture();
    w(
        &f.root.join("src/lib.rs"),
        "pub mod target;\npub mod user;\n",
    );
    w(
        &f.root.join("src/target.rs"),
        "pub fn other() -> u32 {\n    1\n}\n",
    );
    w(
        &f.root.join("src/user.rs"),
        "use crate::target;\npub fn u() -> u32 {\n    target::other()\n}\n",
    );
    f.indexed();
    for args in [&["target"][..], &["-i", "Target"][..]] {
        let mut a = vec!["impact"];
        a.extend_from_slice(args);
        a.extend(["--json=greeg", "--fresh", "stat"]);
        let d = json_lines(&f.out(&a))[1]["data"].clone();
        let likely: Vec<&str> = d["likely"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["path"]["text"].as_str().unwrap())
            .collect();
        assert!(likely.contains(&"src/user.rs"), "{args:?} {d}");
    }
}

/// A context line never carries a UTF-8 BOM, as a matched line never does.
#[test]
fn a_context_line_after_a_bom_holds_only_its_text() {
    let f = empty_fixture();
    fs::write(f.root.join("bom.txt"), b"\xef\xbb\xbfx\nneedle\n").unwrap();
    let out = f.out(&[
        "--json=legacy",
        "--no-index",
        "-B",
        "1",
        "needle",
        "bom.txt",
    ]);
    assert!(
        out.contains(r#""lines":{"text":"x\n"},"line_number":1,"absolute_offset":3"#),
        "{out}"
    );
}

/// `--sort path` orders as `rg --sort path` does: by name within each
/// directory, so `a/x` comes before `a-b/x`.
#[test]
fn path_order_follows_names_within_each_directory() {
    let f = empty_fixture();
    for p in ["a/x.txt", "a-b/x.txt", "a.txt"] {
        w(&f.root.join(p), "needle\n");
    }
    f.indexed();
    for backend in [&["--no-index"][..], &[][..]] {
        let mut args = vec!["-l", "--sort", "path", "needle"];
        args.extend_from_slice(backend);
        assert_eq!(f.out(&args), "a/x.txt\na-b/x.txt\na.txt\n", "{backend:?}");
    }
}

fn json_lines(out: &str) -> Vec<serde_json::Value> {
    out.lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect()
}

/// `--json=greeg` search records, schema 2: a header, then per file a
/// `begin` stating encoding and coordinates and its lines without their
/// terminators, offsets in those coordinates, content as `{"text"}` or
/// `{"bytes"}`.
#[test]
fn json_greeg_search_records_follow_schema_2() {
    let f = empty_fixture();
    fs::write(f.root.join("a.txt"), "a needle\nb\r\nneedle needle\n").unwrap();
    fs::write(f.root.join("bad.txt"), b"inv\xffneedle\n").unwrap();
    fs::write(f.root.join("bom.txt"), b"\xef\xbb\xbfx\nneedle\n").unwrap();
    fs::write(
        f.root.join("u16.txt"),
        b"\xff\xfen\x00e\x00e\x00d\x00l\x00e\x00\n\x00",
    )
    .unwrap();
    let args = [
        "--json=greeg",
        "--no-index",
        "--budget",
        "0",
        "-B",
        "1",
        "needle",
    ];
    let mut records = json_lines(&f.out(&args));
    let footer = records.pop().unwrap();
    for r in &mut records {
        r["data"].as_object_mut().unwrap().shift_remove("score");
    }
    let got: Vec<String> = records.iter().map(|r| r.to_string()).collect();
    let header = format!(
        r#"{{"type":"greeg","data":{{"schema":2,"dialect":"greeg","command":"search","version":"{}"}}}}"#,
        env!("CARGO_PKG_VERSION")
    );
    let want = [
        header.as_str(),
        r#"{"type":"begin","data":{"path":{"text":"a.txt"},"encoding":"utf-8","coordinates":"bytes","file_flags":[],"matched_lines":2}}"#,
        r#"{"type":"match","data":{"line":1,"byte_offset":0,"text":{"text":"a needle"},"submatches":[[2,8]],"kind":"ident","symbol":null,"clipped":false}}"#,
        r#"{"type":"context","data":{"line":2,"byte_offset":9,"text":{"text":"b"}}}"#,
        r#"{"type":"match","data":{"line":3,"byte_offset":12,"text":{"text":"needle needle"},"submatches":[[0,6],[7,13]],"kind":"ident","symbol":null,"clipped":false}}"#,
        r#"{"type":"begin","data":{"path":{"text":"bad.txt"},"encoding":"utf-8","coordinates":"bytes","file_flags":[],"matched_lines":1}}"#,
        r#"{"type":"match","data":{"line":1,"byte_offset":0,"text":{"bytes":"aW52/25lZWRsZQ=="},"submatches":[[4,10]],"kind":"ident","symbol":null,"clipped":false}}"#,
        r#"{"type":"begin","data":{"path":{"text":"bom.txt"},"encoding":"utf-8-bom","coordinates":"bytes","file_flags":[],"matched_lines":1}}"#,
        r#"{"type":"context","data":{"line":1,"byte_offset":3,"text":{"text":"x"}}}"#,
        r#"{"type":"match","data":{"line":2,"byte_offset":5,"text":{"text":"needle"},"submatches":[[0,6]],"kind":"ident","symbol":null,"clipped":false}}"#,
        r#"{"type":"begin","data":{"path":{"text":"u16.txt"},"encoding":"utf-16le","coordinates":"decoded","file_flags":[],"matched_lines":1}}"#,
        r#"{"type":"match","data":{"line":1,"byte_offset":0,"text":{"text":"needle"},"submatches":[[0,6]],"kind":"ident","symbol":null,"clipped":false}}"#,
    ];
    assert_eq!(got, want);
    assert_eq!(footer["type"], "footer");
    assert_eq!(
        footer["data"]["outcome"].to_string(),
        r#"{"exit":0,"exact":true,"rung":"exact","total":5,"shown":5,"complete":true,"source":"scan","fresh":"","deferred":0}"#
    );
    let files = |args: &[&str]| -> Vec<String> {
        json_lines(&f.out(args))
            .iter()
            .filter(|r| r["type"] == "file")
            .map(|r| r["data"].to_string())
            .collect()
    };
    assert_eq!(
        files(&["--json=greeg", "--no-index", "-c", "needle", "a.txt"]),
        [r#"{"path":{"text":"a.txt"},"count":2}"#]
    );
    assert_eq!(
        files(&["--json=greeg", "--no-index", "-l", "needle", "a.txt"]),
        [r#"{"path":{"text":"a.txt"}}"#]
    );
}

/// Every path in `v` is `{"text"}` or `{"bytes"}`, never a bare string.
fn assert_paths_are_text(v: &serde_json::Value, at: &str) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, x) in m {
                if k == "path" || k == "imported_by" && x.is_array() {
                    let items = x.as_array().cloned().unwrap_or_else(|| vec![x.clone()]);
                    for p in items {
                        let o = p.as_object().unwrap_or_else(|| panic!("{at}: {k} {p}"));
                        assert!(
                            o.len() == 1 && (o.contains_key("text") || o.contains_key("bytes")),
                            "{at}: {k} {p}"
                        );
                    }
                } else {
                    assert_paths_are_text(x, at);
                }
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| assert_paths_are_text(x, at)),
        _ => {}
    }
}

/// Every `--json=greeg` answer, search or verb, in both backends: a header
/// naming the command, `{"type","data"}` records only, paths as text, and a
/// footer last whose `outcome.exit` is the status the run returns.
#[test]
fn every_json_greeg_answer_is_typed_and_ends_with_its_outcome() {
    let f = empty_fixture();
    w(
        &f.root.join("src/lib.rs"),
        "pub trait Shape {}\npub struct Square;\nimpl Shape for Square {}\n\
         pub fn needle() -> u32 { 1 }\nfn caller() -> u32 { needle() }\n",
    );
    w(
        &f.root.join("src/use.rs"),
        "use crate::needle;\nfn more() { needle(); }\n",
    );
    f.indexed();
    let cases: &[&[&str]] = &[
        &["needle"],
        &["-C", "1", "needle"],
        &["-l", "needle"],
        &["-c", "needle"],
        &["--budget", "0", "needle"],
        &["absent_zzz"],
        &["NEEDLE"],
        &["def", "needle"],
        &["def", "absent_zzz"],
        &["refs", "needle"],
        &["callers", "needle"],
        &["impls", "Shape"],
        &["impls", "absent_zzz"],
        &["impact", "needle"],
        &["show", "src/lib.rs:5"],
        &["outline", "src/lib.rs"],
        &["map", "."],
    ];
    for backend in [&["--no-index"][..], &[][..]] {
        for case in cases {
            // `map` reads the import graph, which only the index holds
            if case[0] == "map" && !backend.is_empty() {
                continue;
            }
            let mut args = vec!["--json=greeg"];
            args.extend_from_slice(case);
            args.extend_from_slice(backend);
            let o = f.run(&args);
            let at = format!("{args:?}");
            let records = json_lines(&String::from_utf8_lossy(&o.stdout));
            let command = match case[0] {
                "def" | "refs" | "callers" | "impls" | "impact" | "show" | "outline" | "map" => {
                    case[0]
                }
                _ => "search",
            };
            assert!(
                !records.is_empty(),
                "{at}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            assert_eq!(records[0]["type"], "greeg", "{at}");
            assert_eq!(records[0]["data"]["schema"], 2, "{at}");
            assert_eq!(records[0]["data"]["command"], command, "{at}");
            for r in &records {
                let m = r.as_object().unwrap();
                assert!(
                    m.len() == 2 && m["type"].is_string() && m["data"].is_object(),
                    "{at}: {r}"
                );
                assert_paths_are_text(r, &at);
            }
            let footer = records.last().unwrap();
            assert_eq!(footer["type"], "footer", "{at}");
            assert_eq!(
                footer["data"]["outcome"]["exit"].as_i64().map(|e| e as i32),
                o.status.code(),
                "{at}: {footer}"
            );
            assert_eq!(
                records.iter().filter(|r| r["type"] == "footer").count(),
                1,
                "{at}"
            );
        }
    }
}

/// `--json=greeg` reads stdin like a file, and commands that are not
/// answers refuse it.
#[test]
fn json_greeg_covers_stdin_and_refuses_other_commands() {
    use std::io::Write;
    use std::process::Stdio;
    let mut c = greeg()
        .args(["--no-session", "--json=greeg", "alpha"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GREEG_STATS", "0")
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(b"alpha\nbeta\n").unwrap();
    let o = c.wait_with_output().unwrap();
    let records = json_lines(&String::from_utf8_lossy(&o.stdout));
    assert_eq!(records[0]["data"]["command"], "search");
    assert_eq!(records[1]["data"]["path"]["text"], "<stdin>", "{records:?}");
    assert_eq!(records[2]["data"]["text"]["text"], "alpha", "{records:?}");
    let f = empty_fixture();
    for args in [
        &["stats", "--json=greeg"][..],
        &["index", "--json=greeg"][..],
    ] {
        let o = f.run(args);
        assert_eq!(
            (o.status.code(), o.stdout.is_empty()),
            (Some(2), true),
            "{args:?}"
        );
    }
}

/// Run greeg with stderr on a terminal (a pty); returns stdout, stderr and
/// the exit status.
fn run_on_terminal(
    f: &Fixture,
    args: &[&str],
    env: &[(&str, &str)],
) -> (String, String, Option<i32>) {
    use std::io::Read;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::process::Stdio;
    let (mut master, mut slave) = (0, 0);
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(r, 0, "openpty");
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    let mut cmd = greeg();
    cmd.args(args)
        .args(["--no-session", "--index-dir"])
        .arg(&f.index)
        .current_dir(&f.root)
        .env("GREEG_STATS", "0")
        .env("GREEG_INDEX_DIR", &f.index)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(slave));
    let child = cmd.spawn().expect("run greeg");
    drop(cmd);
    // read while greeg runs: a terminal may drop what nobody read once it closes
    let reader = std::thread::spawn(move || {
        let mut err = Vec::new();
        // ends with an error once greeg, the last writer, exits
        let _ = std::fs::File::from(master).read_to_end(&mut err);
        err
    });
    let o = child.wait_with_output().unwrap();
    let err = reader.join().unwrap();
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&err).into_owned(),
        o.status.code(),
    )
}

/// Bare `--json` is `legacy` in this release. On a terminal it says, once,
/// that it becomes `greeg` in 0.11; a pipe, `--no-messages`, a named dialect and
/// commands that keep their own JSON hear nothing.
#[test]
fn bare_json_is_legacy_and_names_its_change_only_to_a_person() {
    let f = empty_fixture();
    w(
        &f.root.join("src/lib.rs"),
        "pub fn needle() {}\nfn caller() { needle(); }\n",
    );
    f.indexed();
    let elapsed = |s: &str| -> String {
        s.lines()
            .map(|l| {
                let mut v: serde_json::Value = serde_json::from_str(l).unwrap();
                if let Some(d) = v.get_mut("data").and_then(|d| d.as_object_mut()) {
                    for k in ["elapsed_total", "elapsed_ms"] {
                        d.shift_remove(k);
                    }
                    if let Some(s) = d.get_mut("stats").and_then(|s| s.as_object_mut()) {
                        s.shift_remove("elapsed");
                    }
                }
                v.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    for args in [&["needle"][..], &["def", "needle"][..]] {
        let bare = f.run(&[&["--json"][..], args].concat());
        let legacy = f.run(&[&["--json=legacy"][..], args].concat());
        assert_eq!(bare.status.code(), legacy.status.code(), "{args:?}");
        assert_eq!(
            elapsed(&String::from_utf8_lossy(&bare.stdout)),
            elapsed(&String::from_utf8_lossy(&legacy.stdout)),
            "{args:?}"
        );
        assert!(
            bare.stderr.is_empty(),
            "{args:?}: {}",
            String::from_utf8_lossy(&bare.stderr)
        );
    }
    let notice = "greeg: --json will mean --json=greeg in 0.11; --json=legacy keeps this output";
    for args in [&["--json", "needle"][..], &["--json", "def", "needle"][..]] {
        let (out, err, code) = run_on_terminal(&f, args, &[]);
        assert_eq!(code, Some(0), "{args:?}: {err}");
        assert!(!json_lines(&out).is_empty(), "{args:?}: {out}");
        assert_eq!(err.matches(notice).count(), 1, "{args:?}: {err}");
    }
    for args in [
        &["--json=legacy", "needle"][..],
        &["--json=greeg", "needle"][..],
        &["--json", "--no-messages", "needle"][..],
        &["--json", "stats"][..],
    ] {
        let (_, err, _) = run_on_terminal(&f, args, &[]);
        assert!(!err.contains("--json will mean"), "{args:?}: {err}");
    }
    // a truncated index file re-runs greeg once; the notice is not repeated
    let (out, err, code) = run_on_terminal(
        &f,
        &["--json", "needle"],
        &[("GREEG_DEBUG_SIGBUS", "index")],
    );
    assert_eq!(code, Some(0), "{err}");
    assert!(err.contains("answering from a scan"), "{err}");
    assert!(!json_lines(&out).is_empty(), "{out}");
    assert_eq!(err.matches(notice).count(), 1, "{err}");
}

/// The FSEvents check answers edits made just before it from the event log,
/// and its statistics say so; when the log is unreliable (dropped events), the
/// stat pass answers. Every answer equals a scan's.
#[cfg(target_os = "macos")]
#[test]
fn fsevents_answers_edits_and_falls_back_to_stat_when_events_are_lost() {
    let f = empty_fixture();
    // edits stay under the delta threshold (5 % of files)
    for d in 0..40 {
        for i in 0..25 {
            let body = if (d + i) % 7 == 0 {
                "fn needle() {}\n"
            } else {
                "fn hay() {}\n"
            };
            w(&f.root.join(format!("d{d}/f{i}.rs")), body);
        }
    }
    for i in 0..3 {
        w(&f.root.join(format!("small/f{i}.rs")), "fn needle() {}\n");
    }
    f.indexed();
    let lines = |o: &Output| -> Vec<String> {
        let mut v: Vec<String> = String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        v.sort();
        v
    };
    let check = |round: usize, lost: bool| {
        let mut env: Vec<(&str, &std::ffi::OsStr)> = vec![(
            "GREEG_DEBUG_FSEVENTS_CUTOFF_MS",
            std::ffi::OsStr::new("5000"),
        )];
        if lost {
            env.push(("GREEG_DEBUG_FSEVENTS_LOST", std::ffi::OsStr::new("1")));
        }
        let o = f.run_env(
            &["needle", "--fresh", "fsevents", "--budget", "0", "--stats"],
            &env,
        );
        let scan = f.run(&["needle", "--no-index", "--budget", "0"]);
        assert_eq!(lines(&o), lines(&scan), "round {round}, lost {lost}");
        let err = String::from_utf8_lossy(&o.stderr).into_owned();
        let method = if lost { "stat" } else { "fsevents" };
        assert!(
            err.contains(&format!("fresh {method} ")),
            "round {round}, lost {lost}: {err}"
        );
    };
    // modify, add a directory, delete, rename a directory; each query runs
    // right after its edits
    w(&f.root.join("d1/f1.rs"), "fn needle() {}\n");
    w(&f.root.join("new/f.rs"), "fn needle() {}\n");
    fs::remove_file(f.root.join("d0/f0.rs")).unwrap();
    fs::rename(f.root.join("small"), f.root.join("small2")).unwrap();
    check(0, false);
    for round in 1..6 {
        w(&f.root.join(format!("d{round}/n.rs")), "fn needle() {}\n");
        let p = f.root.join(format!("d{}/f{round}.rs", round + 10));
        let body = fs::read_to_string(&p).unwrap();
        w(&p, &format!("{body}fn needle() {{}}\n"));
        check(round, false);
    }
    w(&f.root.join("d2/f2.rs"), "fn needle() {}\n");
    fs::remove_file(f.root.join("d3/f4.rs")).unwrap();
    check(6, true);
}
