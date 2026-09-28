//! Every index snapshot is closed before the answer reaches stdout
//! (`commit.rs`). A separate test binary: committing is process-wide. Debug
//! builds enforce it, so every CLI test checks it for the verb it runs.

#![cfg(debug_assertions)]

use greeg_index::build::{BuildOpts, build};
use greeg_index::{Index, commit};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};

#[test]
fn the_index_is_not_used_after_output_started() {
    // created here, so no file left by another run joins the fixture
    let base = (0..)
        .map(|n| std::env::temp_dir().join(format!("greeg-commit-{}-{n}", std::process::id())))
        .find(|d| match fs::create_dir(d) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => panic!("create {}: {e}", d.display()),
        })
        .unwrap();
    let (root, dir) = (base.join("tree"), base.join("index"));
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("lib.rs"), "pub fn alpha_helper() {}\n").unwrap();
    let opts = BuildOpts {
        reader_threads: 1,
        quiet: true,
        ..Default::default()
    };
    build(&root, &dir, &opts).unwrap();
    let idx = Index::open(&dir).unwrap();
    assert_eq!(idx.lookup("alpha_helper").len(), 1);

    commit::commit();
    assert!(
        catch_unwind(AssertUnwindSafe(|| idx.lookup("alpha_helper"))).is_err(),
        "a snapshot open before output was read after it"
    );
    assert!(
        catch_unwind(|| Index::open(&dir)).is_err(),
        "an index was opened after output started"
    );
    let _ = fs::remove_dir_all(&base);
}
