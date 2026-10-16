//! A command the hook rewrites reads back as the words it was given: each
//! rewritten command, split as a shell would, is its rewritten argv, and the
//! rewritten line is the original with only those commands replaced, in order.
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
    // restore each original command: only the rewritten ones may have changed
    let mut parts = rw.parts.iter();
    let mut rebuilt = String::new();
    let mut at = 0;
    for (b, a) in before.into_iter().zip(after) {
        let (_, b_body) = rewrite::trim_blanks(&cmd[b]);
        let (a_lead, a_body) = rewrite::trim_blanks(&rw.command[a.clone()]);
        if a_body != b_body {
            let p = parts.next().expect("a changed command is a part");
            assert_eq!(a_body, p.command, "{cmd:?}");
            rebuilt.push_str(&rw.command[at..a.start + a_lead]);
            rebuilt.push_str(b_body);
            at = a.start + a_lead + a_body.len();
        }
    }
    rebuilt.push_str(&rw.command[at..]);
    assert_eq!(rebuilt, cmd, "only the rewritten commands change");
    assert!(parts.next().is_none(), "{cmd:?}: a part is not in the line");
});
