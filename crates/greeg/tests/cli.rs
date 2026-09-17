//! End-to-end tests of the shipped contract, over a fixture tree in a
//! tempdir, under plain `cargo test`.
//!
//! What lives here and what does not. `bench/parity.py` checks `(path, line)`
//! parity against a real ripgrep over the fetched corpora; it needs the
//! network and only runs in the `e2e` CI job. These tests need neither, and
//! they assert on the output contract in ARCHITECTURE.md rather than on
//! ripgrep: footer shape, demotion, `related`, `-l`/`-c` stream shape, exit codes,
//! rejected flags, and the one property that catches most index bugs without
//! any golden text — **an indexed answer equals a scan-mode answer**.
//!
//! Every test gets its own index directory and no session file, so they are
//! hermetic and can run in parallel, and `GREEG_STATS=0` keeps them out of
//! the user's own stats.

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
    let base = std::env::temp_dir().join(format!(
        "greeg-cli-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let root = base.join("tree");
    let index = base.join("index");
    fs::create_dir_all(&root).unwrap();
    // `.gitignore` only applies inside a git repository, as in ripgrep
    fs::create_dir_all(root.join(".git")).unwrap();

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
// footer contract (ARCHITECTURE.md, "What the output means")
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
    let o = f.run(&["--no-ladder", "zzz_no_such_identifier", "src"]);
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
    let whole = f.out(&["--no-ladder", "zzz_no_such_identifier"]);
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
