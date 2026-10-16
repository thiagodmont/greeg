//! A command the hook rewrites reads back as the words it was given: each
//! rewritten command, split as a shell would, is its rewritten argv, and it
//! appears in the rewritten line.
#![no_main]

#[allow(dead_code)]
#[path = "../../crates/greeg/src/rewrite.rs"]
mod rewrite;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(cmd) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(rw) = rewrite::rewrite_full(cmd) {
        for p in &rw.parts {
            assert_eq!(
                rewrite::shell_words(&p.command).as_deref(),
                Ok(&p.rewritten[..]),
                "{cmd:?}"
            );
            assert!(rw.command.contains(&p.command), "{cmd:?}");
        }
    }
});
