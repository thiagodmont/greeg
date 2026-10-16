//! A command the hook rewrites reads back as the words it was given: each
//! rewritten command, split as a shell would, is its rewritten argv, and the
//! rewritten line has the same commands in the same order, the others byte
//! for byte.
#![no_main]

#[allow(dead_code)]
#[path = "../../crates/greeg/src/rewrite.rs"]
mod rewrite;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(cmd) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(rw) = rewrite::rewrite_full(cmd) else {
        return;
    };
    for p in &rw.parts {
        assert_eq!(
            rewrite::shell_words(&p.command).as_deref(),
            Ok(&p.rewritten[..]),
            "{cmd:?}"
        );
    }
    let before = rewrite::list_segments(cmd).expect("a rewritten line splits");
    let after = rewrite::list_segments(&rw.command).expect("the rewrite splits");
    assert_eq!(before.len(), after.len(), "{cmd:?}");
    let mut parts = rw.parts.iter();
    for (b, a) in before.into_iter().zip(after) {
        let (b_lead, b_body) = rewrite::trim_blanks(&cmd[b.clone()]);
        let (a_lead, a_body) = rewrite::trim_blanks(&rw.command[a.clone()]);
        assert_eq!(cmd[b.start..b.start + b_lead], rw.command[a.start..a.start + a_lead]);
        assert_eq!(
            cmd[b.start + b_lead + b_body.len()..b.end],
            rw.command[a.start + a_lead + a_body.len()..a.end]
        );
        if a_body != b_body {
            let p = parts.next().expect("a changed command is a part");
            assert_eq!(a_body, p.command, "{cmd:?}");
        }
    }
    assert!(parts.next().is_none(), "{cmd:?}: a part is not in the line");
});
