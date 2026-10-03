//! A command the hook rewrites reads back as the words it was given: the
//! rewritten command, split as a shell would, is the rewritten argv.
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
        assert_eq!(rw.original.first().map(String::as_str), Some("rg"));
        assert_eq!(rw.rewritten.first().map(String::as_str), Some("greeg"));
        assert_eq!(
            rewrite::shell_words(&rw.command).as_deref(),
            Ok(&rw.rewritten[..]),
            "{cmd:?}"
        );
    }
});
