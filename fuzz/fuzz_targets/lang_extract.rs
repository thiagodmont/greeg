//! Lexing and symbol extraction take any bytes: spans are sorted, disjoint
//! and inside the source, and every symbol lies inside it. Input: a selector
//! byte (language; the high bit asks for TSX), then the source.
#![no_main]

use greeg_lang::Lang;
use libfuzzer_sys::fuzz_target;

const LANGS: [Lang; 5] = [
    Lang::Python,
    Lang::Rust,
    Lang::JavaScript,
    Lang::TypeScript,
    Lang::Kotlin,
];

fuzz_target!(|data: &[u8]| {
    let Some((&sel, src)) = data.split_first() else {
        return;
    };
    let lang = LANGS[(sel & 0x7f) as usize % LANGS.len()];
    let len = src.len() as u32;
    let lexed = greeg_lang::lexer::lex(lang, src);
    let mut end = 0;
    for s in &lexed.spans {
        assert!(end <= s.start && s.start < s.end && s.end <= len, "{s:?}");
        end = s.end;
    }
    let x = greeg_lang::sym::extract(lang, sel & 0x80 != 0, src);
    for s in &x.symbols {
        assert!(s.start <= s.end && s.end <= len, "{}..{}", s.start, s.end);
        assert!(s.name_start <= s.name_end && s.name_end <= len);
    }
    for s in &x.noncode {
        assert!(s.start <= s.end && s.end <= len, "{s:?}");
    }
    for i in &x.imports {
        assert!(i.start <= i.end && i.end <= len);
    }
});
