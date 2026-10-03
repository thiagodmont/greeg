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
