//! S3: Kotlin grammar (tree-sitter-kotlin-sg 0.4.1) — parse speed, ERROR rate, tag coverage vs a regex oracle.
//! usage: s3-kotlin ROOT [ROOT...]
use anyhow::Result;
use rayon::prelude::*;
use regex::Regex;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

const TAGS: &str = r#"
(class_declaration (type_identifier) @name) @definition.class
(object_declaration (type_identifier) @name) @definition.object
(companion_object (type_identifier) @name) @definition.object
(function_declaration (simple_identifier) @name) @definition.function
(property_declaration (variable_declaration (simple_identifier) @name)) @definition.property
(property_declaration (multi_variable_declaration (variable_declaration (simple_identifier) @name))) @definition.property
(type_alias (type_identifier) @name) @definition.typealias
(enum_entry (simple_identifier) @name) @definition.enum_member
(class_parameter (simple_identifier) @name) @definition.param
(secondary_constructor) @definition.constructor
"#;

#[derive(Default)]
struct Stats {
    files: usize, bytes: u64, parse: Duration, error_files: usize, bad_files: usize, error_nodes: usize,
    tags: BTreeMap<String, usize>, oracle_top: usize, tags_top: usize, tags_top_matched: usize,
    samples: Vec<String>, worst: Vec<(String, usize, usize)>,
}

fn count_errors(n: Node, acc: &mut (usize, usize, Vec<(usize, usize)>)) {
    if n.is_error() || n.is_missing() {
        acc.0 += 1; acc.1 += n.byte_range().len(); if acc.2.len() < 3 { acc.2.push((n.start_byte(), n.end_byte())); }
        return;
    }
    if !n.has_error() { return; }
    let mut c = n.walk();
    for ch in n.children(&mut c) { count_errors(ch, acc); }
}

fn main() -> Result<()> {
    let roots: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    let lang: tree_sitter::Language = tree_sitter_kotlin_sg::LANGUAGE.into();
    let query = Query::new(&lang, TAGS)?;
    let name_idx = query.capture_index_for_name("name").unwrap();
    let cap_names: Vec<String> = query.capture_names().iter().map(|s| s.to_string()).collect();
    // oracle: declarations at indent <= 4 (top-level + direct members), modifiers allowed
    let oracle = Regex::new(r"(?m)^ {0,4}(?:(?:public|private|protected|internal|open|abstract|final|sealed|data|enum|annotation|inner|inline|value|suspend|operator|infix|override|external|tailrec|actual|expect|lateinit|const|companion|vararg|crossinline|noinline)\s+)*(?:fun|class|object|interface|val|var|typealias|constructor)\b")?;
    for root in &roots {
        let files: Vec<PathBuf> = ignore::WalkBuilder::new(root).hidden(true).build().filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "kt" || x == "kts").unwrap_or(false)).map(|e| e.into_path()).collect();
        let st = Mutex::new(Stats::default());
        let t0 = Instant::now();
        files.par_iter().for_each(|p| {
            let Ok(src) = fs::read(p) else { return };
            let mut parser = Parser::new();
            parser.set_language(&lang).unwrap();
            let t = Instant::now();
            let Some(tree) = parser.parse(&src, None) else { return };
            let dt = t.elapsed();
            let root_node = tree.root_node();
            let mut errs = (0usize, 0usize, vec![]);
            if root_node.has_error() { count_errors(root_node, &mut errs); }
            let mut local: BTreeMap<String, usize> = BTreeMap::new();
            let mut tags_top = 0usize;
            let mut cursor = QueryCursor::new();
            let mut it = cursor.matches(&query, root_node, src.as_slice());
            while let Some(m) = it.next() {
                let def = m.captures().iter().find(|c| c.index != name_idx).unwrap();
                let kind = cap_names[def.index as usize].trim_start_matches("definition.").to_string();
                *local.entry(kind.clone()).or_default() += 1;
                if def.node.start_position().column <= 4 && kind != "param" { tags_top += 1; }
            }
            let text = String::from_utf8_lossy(&src);
            let oracle_n = oracle.find_iter(&text).count();
            let mut s = st.lock().unwrap();
            s.files += 1; s.bytes += src.len() as u64; s.parse += dt; s.error_nodes += errs.0;
            if errs.0 > 0 { s.error_files += 1; }
            if errs.1 * 5 > src.len() { s.bad_files += 1; }
            for (k, v) in local { *s.tags.entry(k).or_default() += v; }
            s.oracle_top += oracle_n; s.tags_top += tags_top; s.tags_top_matched += tags_top.min(oracle_n);
            if s.samples.len() < 10 {
                for (a, b) in errs.2.iter().take(1) {
                    let line = text[..*a].matches('\n').count() + 1;
                    let snip: String = text[*a..(*b).min(a + 90)].replace('\n', "⏎");
                    s.samples.push(format!("{}:{}: {}", p.strip_prefix(root).unwrap_or(p).display(), line, snip));
                }
            }
            if (oracle_n as i64 - tags_top as i64).abs() >= 8 { s.worst.push((p.strip_prefix(root).unwrap_or(p).display().to_string(), oracle_n, tags_top)); }
        });
        let wall = t0.elapsed();
        let s = st.into_inner().unwrap();
        let mb = s.bytes as f64 / 1e6;
        println!("== {} : {} files, {:.1} MB, wall {:.0} ms ({} threads), parse cpu {:.2} s => {:.1} MB/s/core, wall {:.0} MB/s",
            root.display(), s.files, mb, wall.as_secs_f64() * 1e3, rayon::current_num_threads(), s.parse.as_secs_f64(), mb / s.parse.as_secs_f64(), mb / wall.as_secs_f64());
        println!("   files with ERROR/MISSING: {} ({:.1}%), files with >20% error bytes: {}, error nodes: {}",
            s.error_files, 100.0 * s.error_files as f64 / s.files as f64, s.bad_files, s.error_nodes);
        println!("   tags: {:?}", s.tags);
        println!("   coverage (indent<=4 decls): oracle={} tags={} min-matched={} => {:.1}%", s.oracle_top, s.tags_top, s.tags_top_matched, 100.0 * s.tags_top_matched as f64 / s.oracle_top as f64);
        for x in &s.samples { println!("   ERR {}", x); }
        let mut w = s.worst.clone(); w.sort_by_key(|(_, o, t)| -((*o as i64 - *t as i64).abs())); 
        for (p, o, t) in w.iter().take(6) { println!("   MISMATCH {} oracle={} tags={}", p, o, t); }
    }
    Ok(())
}
