//! Token estimation for the footer's `~N tokens` and the shaper's budget
//! accounting (DESIGN.md §6.4, OUTPUT.md "Token estimate").
//!
//! A byte-feature model fitted against o200k_base on 20 greeg text outputs
//! (five corpora, content/facets/outline/block layouts): word pieces split at
//! `_` and case changes, bucketed by length and case, digit groups,
//! punctuation characters and runs, non-ASCII characters. Fitted ratio
//! estimate/o200k: min 0.945, max 1.054, mean 1.00 (measured 0.98–1.06 on the review query set after fitting).
//! Plain byte divisors were off by up to 15 % because Rust output tokenizes
//! at ~3.0 bytes/token and Kotlin/Python prose at ~4.0.

/// Estimated o200k tokens of a rendered text.
pub fn estimate(b: &[u8]) -> usize {
    let (
        mut lo_short,
        mut lo_mid,
        mut lo_long,
        mut up,
        mut digits,
        mut punct,
        mut pruns,
        mut nonascii,
        mut under,
    ) = (0f64, 0f64, 0f64, 0f64, 0f64, 0f64, 0f64, 0f64, 0f64);
    let n = b.len();
    let mut i = 0;
    let mut in_punct = false;
    while i < n {
        let c = b[i];
        if c.is_ascii_alphabetic() {
            in_punct = false;
            let start = i;
            while i < n && b[i].is_ascii_alphabetic() {
                i += 1;
            }
            // split the run into pieces: `HTTPResponse` → HTTP, Response; `getQuerySet` → get, Query, Set
            let mut j = start;
            while j < i {
                let ps = j;
                let mut upper_led = false;
                if b[j].is_ascii_uppercase() {
                    upper_led = true;
                    let us = j;
                    while j < i && b[j].is_ascii_uppercase() {
                        j += 1;
                    }
                    if j < i && b[j].is_ascii_lowercase() {
                        // the last capital starts the next piece unless it is the only one
                        if j - us > 1 {
                            j -= 1;
                        } else {
                            while j < i && b[j].is_ascii_lowercase() {
                                j += 1;
                            }
                        }
                    }
                } else {
                    while j < i && b[j].is_ascii_lowercase() {
                        j += 1;
                    }
                }
                let len = j - ps;
                if upper_led {
                    up += 1.0;
                } else if len <= 3 {
                    lo_short += 1.0;
                } else if len <= 8 {
                    lo_mid += 1.0;
                } else {
                    lo_long += 1.0;
                }
            }
        } else if c.is_ascii_digit() {
            in_punct = false;
            let start = i;
            while i < n && b[i].is_ascii_digit() {
                i += 1;
            }
            digits += (i - start).div_ceil(3) as f64;
        } else if c >= 0xC0 {
            in_punct = false;
            nonascii += 1.0;
            i += 1;
        } else if c >= 0x80 || c.is_ascii_whitespace() {
            in_punct = false;
            i += 1;
        } else {
            if c == b'_' {
                under += 1.0;
            }
            punct += 1.0;
            if !in_punct {
                pruns += 1.0;
                in_punct = true;
            }
            i += 1;
        }
    }
    let t = 1.822 * lo_short
        + 0.717 * lo_mid
        + 4.489 * lo_long
        + 1.009 * up
        + 1.797 * digits
        + 0.203 * punct
        + 0.515 * pruns
        + 3.354 * nonascii
        - 0.449 * under;
    t.ceil().max(0.0) as usize
}

/// Code text (a hit line, a context line).
pub fn code(bytes: &[u8]) -> usize {
    estimate(bytes)
}
/// A path.
pub fn path(bytes: &[u8]) -> usize {
    estimate(bytes)
}
/// A fully rendered text output.
pub fn rendered(bytes: &[u8]) -> usize {
    estimate(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tracks_rough_token_counts() {
        // 4 short words, two spaces: about 4–8 tokens
        let t = estimate(b"fn poll_read_ready(&self)");
        assert!((6..=12).contains(&t), "{t}");
        assert_eq!(estimate(b""), 0);
        // camel and snake pieces are counted separately
        assert!(estimate(b"HTTPResponseRedirect") > estimate(b"Redirect"));
        assert!(estimate(b"tokio/src/sync/batch_semaphore.rs") >= 8);
    }
}
