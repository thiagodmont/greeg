//! Token estimation for the footer's `~N tokens` and the budget accounting of
//! search and verb output.
//!
//! o200k_base first splits text into pieces (words with an optional leading
//! space or punctuation byte, 1–3 digit groups, punctuation runs, whitespace
//! runs) and most pieces are one token. [`estimate`] splits the same way for
//! ASCII and weights each piece by kind and length. Fitted against o200k on
//! 716 search and verb outputs, text and JSON, from seven corpora:
//! estimate/o200k is 0.94–1.06 for 90 % of them and 0.83–1.24 at the
//! extremes (short answers whose words the vocabulary splits unusually).

const WORD: f64 = 0.89;
const WORD_LEN: f64 = 0.21;
const CAPS_LEN: f64 = 0.51;
const LEAD: f64 = 0.44;
const SPACE_OR_DIGITS: f64 = 1.11;
const PUNCT: f64 = 0.9;
const PUNCT_LEN: f64 = 0.83;
const NON_ASCII: f64 = 0.6;

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn is_punct(c: u8) -> bool {
    !c.is_ascii_alphanumeric() && !is_space(c)
}

/// The end of the word at `i`: capitals, then lowercase, then a contraction.
fn word_end(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_uppercase() {
        i += 1;
    }
    while i < b.len() && b[i].is_ascii_lowercase() {
        i += 1;
    }
    if b.get(i) == Some(&b'\'') {
        for suffix in [&b"s"[..], b"t", b"m", b"d", b"re", b"ve", b"ll"] {
            if b.len() > i + suffix.len()
                && b[i + 1..=i + suffix.len()].eq_ignore_ascii_case(suffix)
            {
                return i + 1 + suffix.len();
            }
        }
    }
    i
}

/// Timing fields and stand-ins wider than any value they take.
const TIMINGS: [(&[u8], &[u8]); 4] = [
    (b"\"elapsed_ms\":", b"99999.999999999999"),
    (b"\"secs\":", b"99999"),
    (b"\"nanos\":", b"999999999"),
    (b"\"human\":", b"\"99999.999999999s\""),
];

/// `b` with each JSON timing value replaced by a stand-in at least as wide,
/// so a fit measured on it gives the same answer on every run and never
/// measures less than the real output. Only a key after `{` or `,` is one: a
/// quote inside a JSON string is escaped. A value wider than its stand-in is
/// kept.
pub fn timeless(b: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if !TIMINGS
        .iter()
        .any(|(key, _)| memchr::memmem::find(b, key).is_some())
    {
        return std::borrow::Cow::Borrowed(b);
    }
    let mut out = Vec::with_capacity(b.len() + 64);
    let mut i = 0;
    'bytes: while i < b.len() {
        if b[i] == b'"' && i > 0 && matches!(b[i - 1], b'{' | b',') {
            for (key, stand_in) in TIMINGS {
                if b[i..].starts_with(key) {
                    out.extend_from_slice(key);
                    let start = i + key.len();
                    let mut end = start;
                    if b.get(end) == Some(&b'"') {
                        end += 1 + memchr::memchr(b'"', &b[end + 1..])
                            .map_or(b.len() - end - 1, |k| k + 1);
                    } else {
                        while end < b.len()
                            && matches!(b[end], b'0'..=b'9' | b'.' | b'-' | b'+' | b'e' | b'E')
                        {
                            end += 1;
                        }
                    }
                    if end - start > stand_in.len() {
                        out.extend_from_slice(&b[start..end]);
                    } else {
                        out.extend_from_slice(stand_in);
                    }
                    i = end;
                    continue 'bytes;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    std::borrow::Cow::Owned(out)
}

/// Estimated o200k tokens of a rendered text.
pub fn estimate(b: &[u8]) -> usize {
    let n = b.len();
    let mut t = 0f64;
    let mut i = 0;
    while i < n {
        let c = b[i];
        let leads_word = c < 0x80
            && !c.is_ascii_alphanumeric()
            && c != b'\n'
            && c != b'\r'
            && b.get(i + 1).is_some_and(u8::is_ascii_alphabetic);
        if c.is_ascii_alphabetic() || leads_word {
            let start = if leads_word { i + 1 } else { i };
            let end = word_end(b, start);
            let word = &b[start..end];
            t += WORD;
            if word.len() > 1 && word.iter().all(|&w| w.is_ascii_uppercase() || w == b'\'') {
                t += CAPS_LEN * (word.len() - 2) as f64;
            } else {
                t += WORD_LEN * word.len().saturating_sub(6) as f64;
            }
            if leads_word && c != b' ' {
                t += LEAD;
            }
            i = end;
        } else if c.is_ascii_digit() {
            let start = i;
            while i < n && i - start < 3 && b[i].is_ascii_digit() {
                i += 1;
            }
            t += SPACE_OR_DIGITS;
        } else if is_punct(c) || (c == b' ' && b.get(i + 1).is_some_and(|&d| is_punct(d))) {
            if c == b' ' {
                i += 1;
            }
            let (mut ascii, mut non_ascii) = (0usize, 0usize);
            while i < n && is_punct(b[i]) {
                if b[i] < 0x80 {
                    ascii += 1;
                } else if b[i] >= 0xC0 {
                    non_ascii += 1;
                }
                i += 1;
            }
            while i < n && matches!(b[i], b'\n' | b'\r' | b'/') {
                ascii += usize::from(b[i] == b'/');
                i += 1;
            }
            if ascii > 0 {
                t += PUNCT + PUNCT_LEN * ascii.saturating_sub(3) as f64;
            }
            t += NON_ASCII * non_ascii as f64;
        } else {
            // a whitespace run ends at its last line break; otherwise its last
            // byte joins the piece that follows
            let start = i;
            let mut end = i;
            while end < n && is_space(b[end]) {
                end += 1;
            }
            i = match b[start..end]
                .iter()
                .rposition(|&w| w == b'\n' || w == b'\r')
            {
                Some(k) => start + k + 1,
                None if end < n && end - start >= 2 => end - 1,
                None => end,
            };
            t += SPACE_OR_DIGITS;
        }
    }
    t.ceil() as usize
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

    /// Timings are measured at stand-ins wider than any real value.
    #[test]
    fn timeless_ignores_the_clock_and_never_measures_less() {
        let at = |ms: &str, ns: &str| {
            format!(
                r#"{{"type":"summary","data":{{"elapsed_total":{{"secs":0,"nanos":{ns},"human":"0.{ns}s"}}}}}}
{{"type":"footer","data":{{"elapsed_ms":{ms},"text":"\"elapsed_ms\":1"}}}}"#
            )
        };
        let (a, b) = (at("1.864", "1864000"), at("9.909334000000001", "820635292"));
        assert_eq!(timeless(a.as_bytes()), timeless(b.as_bytes()));
        for s in [&a, &b] {
            let t = timeless(s.as_bytes());
            assert!(t.len() >= s.len() && estimate(&t) >= estimate(s.as_bytes()));
            // an escaped key inside a string is text, not a timing
            assert!(String::from_utf8_lossy(&t).contains(r#""text":"\"elapsed_ms\":1""#));
        }
        let plain = b"no timings here";
        assert!(matches!(timeless(plain), std::borrow::Cow::Borrowed(_)));
        // only a key is a timing, and a value wider than its stand-in stays
        for kept in [&b"let x = \"secs\":1;"[..], br#"{"secs":123456789012345}"#] {
            assert_eq!(&*timeless(kept), kept);
        }
    }

    /// A JSON fit renders `est_tokens` once with a stand-in of the same width.
    #[test]
    fn numbers_of_one_width_estimate_alike() {
        let footer = |n: usize| format!(r#"{{"est_tokens":{n},"elapsed_ms":1.864}}"#);
        for (a, b) in [(1000, 1872), (2000, 9999), (300, 153), (7, 1)] {
            assert_eq!(footer(a).len(), footer(b).len());
            assert_eq!(
                estimate(footer(a).as_bytes()),
                estimate(footer(b).as_bytes())
            );
        }
        // another width is rendered again
        for (a, b) in [(99, 100), (999, 1000), (9999, 10000)] {
            assert_ne!(footer(a).len(), footer(b).len());
        }
        assert_ne!(
            estimate(footer(999).as_bytes()),
            estimate(footer(1000).as_bytes())
        );
    }

    /// Verb layouts and JSON lines, with their o200k_base counts.
    #[test]
    fn estimate_is_within_ten_percent_on_verb_layouts_and_json() {
        let outline =
            "outline tokio/src/runtime/blocking/pool.rs  99 symbols · 24 imports · rust · index
  struct BlockingPool  :20  (+2 nested)
  struct Spawner  :26  (+1 nested)
  struct SpawnerMetrics  :31  (+3 nested)
  impl SpawnerMetrics  :37  (+9 nested)
  struct Inner  :77  (+8 nested)
  enum InnerImpl  :104  [doc]  (+2 nested)
  struct LockedImpl  :110  [doc]  (+2 nested)
  type ShutdownHandles  :126  [doc]
  const KEEP_ALIVE  :230
  fn spawn_blocking  :237
  fn spawn_mandatory_blocking  :255  [doc]
  +4 more
12/99 symbols · raise --budget
";
        let json = r#"{"type":"impl","data":{"path":{"text":"tokio/src/io/async_buf_read.rs"},"line":23,"kind":"trait","name":"AsyncBufRead","container":"","signature":"pub trait AsyncBufRead: AsyncRead {","doc":"Reads bytes asynchronously.","flags":["exported"],"supertypes":["AsyncRead"],"score":0.61,"reach":0.6,"start":783,"end":2863,"confidence":"high"}}
{"type":"impl","data":{"path":{"text":"tokio-util/src/compat.rs"},"line":132,"kind":"trait","name":"FuturesAsyncReadCompatExt","container":"","signature":"pub trait FuturesAsyncReadCompatExt: futures_io::AsyncRead {","doc":"Extension trait that allows converting a type implementing","flags":["exported"],"supertypes":["AsyncRead"],"score":0.603,"reach":0.6,"start":4741,"end":5003,"confidence":"high"}}
"#;
        let show = "show tokio/src/runtime/blocking/pool.rs:237  fn spawn_blocking · lines 237-253
237  pub(crate) fn spawn_blocking<F, R>(func: F) -> JoinHandle<R>
238  where
239      F: FnOnce() -> R + Send + 'static,
240      R: Send + 'static,
241  {
242      let rt = Handle::current();
243      rt.spawn_blocking(func)
244  }
";
        for (text, o200k) in [(outline, 174.0), (json, 204.0), (show, 97.0)] {
            let ratio = estimate(text.as_bytes()) as f64 / o200k;
            assert!((0.9..=1.1).contains(&ratio), "{ratio:.3}: {text}");
        }
    }
}
