//! Runtime-loaded extra languages (ARCHITECTURE.md): a directory per language
//! under `$GREEG_LANG_DIR` (default `~/.config/greeg/lang`) holding
//!
//! ```text
//! spec.toml     name, extensions, symbol (default tree_sitter_<name>),
//!               line_comment, block_comment = ["/*", "*/"], strings = ["\"", "'"],
//!               imports = ["import "]
//! grammar.so    (or .dylib) the compiled tree-sitter parser: cc -shared -fPIC -O2 -I src src/parser.c [src/scanner.c]
//! tags.scm      a greeg tags query (captures @def.<kind>, @name, @supers, @noncode.*, @import)
//! ```
//!
//! The registry is read once per process (a directory listing, microseconds);
//! the shared object is `dlopen`ed only when a file of that language is
//! parsed. Extra languages get index-time symbols and noncode spans from
//! their query, byte-rule hit kinds, and a generic comment/string lexer in
//! scan mode; they have no definition regexes and no import resolution.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct ExtraLang {
    pub name: &'static str,
    pub extensions: Vec<String>,
    pub dir: PathBuf,
    pub symbol: String,
    pub line_comment: Option<Vec<u8>>,
    pub block_comment: Option<(Vec<u8>, Vec<u8>)>,
    pub strings: Vec<u8>,
    pub imports: Vec<String>,
}

pub fn lang_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("GREEG_LANG_DIR") {
        return Some(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/greeg/lang"))
}

/// Minimal TOML subset: `key = "string"` and `key = ["a", "b"]`, one per line.
fn parse_spec(s: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim().to_string(), v.trim());
        let vals: Vec<String> =
            if let Some(inner) = v.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
                inner
                    .split(',')
                    .map(|x| unquote(x.trim()))
                    .filter(|x| !x.is_empty())
                    .collect()
            } else {
                vec![unquote(v)]
            };
        out.push((k, vals));
    }
    out
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
        .unwrap_or(s);
    s.replace("\\\\", "\\").replace("\\\"", "\"")
}

fn load_dir(dir: &Path) -> Option<ExtraLang> {
    let spec = std::fs::read_to_string(dir.join("spec.toml")).ok()?;
    let kv = parse_spec(&spec);
    let get = |k: &str| kv.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());
    let name = get("name")
        .and_then(|v| v.into_iter().next())
        .unwrap_or_else(|| {
            dir.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    if name.is_empty() {
        return None;
    }
    let extensions = get("extensions")
        .unwrap_or_default()
        .into_iter()
        .map(|e| e.trim_start_matches('.').to_string())
        .collect();
    let symbol = get("symbol")
        .and_then(|v| v.into_iter().next())
        .unwrap_or_else(|| format!("tree_sitter_{}", name.replace('-', "_")));
    let line_comment = get("line_comment")
        .and_then(|v| v.into_iter().next())
        .map(|s| s.into_bytes())
        .filter(|b| !b.is_empty());
    let block_comment = get("block_comment").and_then(|v| {
        if v.len() == 2 {
            Some((v[0].clone().into_bytes(), v[1].clone().into_bytes()))
        } else {
            None
        }
    });
    let strings: Vec<u8> = get("strings")
        .unwrap_or_else(|| vec!["\"".into()])
        .into_iter()
        .filter_map(|s| s.bytes().next())
        .collect();
    let imports = get("imports").unwrap_or_default();
    Some(ExtraLang {
        name: Box::leak(name.into_boxed_str()),
        extensions,
        dir: dir.to_path_buf(),
        symbol,
        line_comment,
        block_comment,
        strings,
        imports,
    })
}

/// All registered extra languages (index = `Lang::Extra(index)`).
pub fn registry() -> &'static [ExtraLang] {
    static R: OnceLock<Vec<ExtraLang>> = OnceLock::new();
    R.get_or_init(|| {
        let Some(dir) = lang_dir() else {
            return Vec::new();
        };
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut v: Vec<ExtraLang> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| load_dir(&e.path()))
            .collect();
        v.sort_by(|a, b| a.name.cmp(b.name));
        v.truncate(200);
        v
    })
}

pub fn get(i: u8) -> Option<&'static ExtraLang> {
    registry().get(i as usize)
}

pub fn by_extension(ext: &str) -> Option<u8> {
    registry()
        .iter()
        .position(|l| l.extensions.iter().any(|e| e == ext))
        .map(|i| i as u8)
}

pub fn by_name(name: &str) -> Option<u8> {
    registry()
        .iter()
        .position(|l| l.name == name || l.extensions.iter().any(|e| e == name))
        .map(|i| i as u8)
}

/// `dlopen` the grammar and return its `tree_sitter::Language` and the tags query text.
pub fn grammar(i: u8) -> Option<(tree_sitter::Language, String)> {
    let l = get(i)?;
    let so = ["grammar.dylib", "grammar.so"]
        .iter()
        .map(|n| l.dir.join(n))
        .find(|p| p.exists())?;
    let query = std::fs::read_to_string(l.dir.join("tags.scm")).ok()?;
    let path = std::ffi::CString::new(so.to_string_lossy().as_bytes()).ok()?;
    let sym = std::ffi::CString::new(l.symbol.as_bytes()).ok()?;
    // SAFETY: the shared object is a tree-sitter parser compiled by the user; the
    // symbol is a `const TSLanguage *fn(void)` by the tree-sitter ABI contract.
    unsafe {
        let h = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if h.is_null() {
            return None;
        }
        let p = libc::dlsym(h, sym.as_ptr());
        if p.is_null() {
            return None;
        }
        let f: unsafe extern "C" fn() -> *const () = std::mem::transmute(p);
        let lf = tree_sitter_language::LanguageFn::from_raw(f);
        let lang: tree_sitter::Language = lf.into();
        if lang.abi_version() < tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION
            || lang.abi_version() > tree_sitter::LANGUAGE_VERSION
        {
            return None;
        }
        Some((lang, query))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn spec_parsing() {
        let kv = parse_spec(
            "name = \"go\"\nextensions = [\".go\", \"gox\"]\nblock_comment = [\"/*\", \"*/\"]\n# c\n[x]\nstrings = [\"\\\"\", \"`\"]\n",
        );
        assert_eq!(kv[0], ("name".into(), vec!["go".into()]));
        assert_eq!(kv[1].1, vec![".go", "gox"]);
        assert_eq!(kv[3].1, vec!["\"", "`"]);
    }
}
