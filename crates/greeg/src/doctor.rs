//! `greeg doctor`: index health, freshness mode, language coverage, disk use.
//! `greeg lang check DIR`: validate runtime-loaded languages and measure coverage.

use crate::{Common, fmt_n, fmt_size};
use anyhow::Result;
use greeg_lang::{FileFlags, Lang, extra, sym};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

fn ago(unix_ms: u64) -> String {
    let now = greeg_index::now_ms();
    if unix_ms == 0 || now < unix_ms {
        return "never".into();
    }
    let s = (now - unix_ms) / 1000;
    if s < 60 {
        format!("{s} s ago")
    } else if s < 3600 {
        format!("{} min ago", s / 60)
    } else if s < 86400 {
        format!("{} h ago", s / 3600)
    } else {
        format!("{} d ago", s / 86400)
    }
}

fn dir_size(dir: &Path, prefix: &str) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

pub fn run(c: &Common) -> Result<()> {
    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    let root = c.root.clone().unwrap_or_else(|| PathBuf::from("."));
    let exe = std::env::current_exe().ok();
    let exe_size = exe
        .as_ref()
        .and_then(|e| std::fs::metadata(e).ok())
        .map(|m| m.len())
        .unwrap_or(0);
    writeln!(
        w,
        "greeg {}  {} ({})",
        env!("CARGO_PKG_VERSION"),
        exe.as_ref()
            .map(|e| e.display().to_string())
            .unwrap_or_default(),
        fmt_size(exe_size)
    )?;
    let canon = std::fs::canonicalize(&root).unwrap_or(root.clone());
    writeln!(w, "root      {}", canon.display())?;
    let dir = match &c.index_dir {
        Some(d) => d.clone(),
        None => greeg_index::index_dir_for(&root)?,
    };
    let mut advice: Vec<String> = Vec::new();
    let building = dir.join("BUILDING").exists();
    let manifest = greeg_index::read_manifest(&dir);
    match &manifest {
        None => {
            writeln!(
                w,
                "index     none at {}{}",
                dir.display(),
                if building { " (build in progress)" } else { "" }
            )?;
            if !building {
                advice.push("no index yet: the first query builds it in the background, or run `greeg index` now".into());
            }
        }
        Some(m) => {
            let g = m.generation;
            let sz = |p: &str| dir_size(&dir, &format!("{p}.{g}."));
            let (fb, gb, sb, pb, grb) = (
                sz("files"),
                sz("grams"),
                sz("symbols"),
                sz("spans"),
                sz("graph"),
            );
            let deltas = dir_size(&dir.join("delta"), "");
            let total = fb + gb + sb + pb + grb + deltas;
            writeln!(
                w,
                "index     {}  generation {}  format {}  built {} in {:.1} s{}",
                dir.display(),
                g,
                m.format,
                ago(m.built_unix_ms),
                m.build_ms / 1e3,
                if building {
                    "  (rebuild in progress)"
                } else {
                    ""
                }
            )?;
            writeln!(
                w,
                "          phase 1 {} grams {} · phase 2 {} symbols {}, spans {}, graph {} · files {} · deltas {} · total {} ({:.2}× of {} source)",
                if m.phase1 { "✓" } else { "✗" },
                fmt_size(gb),
                if m.phase2 {
                    "✓"
                } else {
                    "✗ (symbols pending)"
                },
                fmt_size(sb),
                fmt_size(pb),
                fmt_size(grb),
                fmt_size(fb),
                fmt_size(deltas),
                fmt_size(total),
                total as f64 / m.source_bytes.max(1) as f64,
                fmt_size(m.source_bytes)
            )?;
            writeln!(
                w,
                "          {} files · {} symbols · {} import edges · {} regex fallbacks · {} delta segments · {} tombstones · verified {}",
                fmt_n(m.files as usize),
                fmt_n(m.symbols as usize),
                fmt_n(m.edges as usize),
                fmt_n(m.parse_fallbacks as usize),
                m.deltas,
                m.tombstones,
                ago(m.verified_unix_ms)
            )?;
            if m.deltas >= 12 {
                advice.push(format!("{} delta segments: a rebuild will be triggered at 16; `greeg index` compacts now", m.deltas));
            }
            if greeg_index::now_ms().saturating_sub(m.built_unix_ms) > 7 * 86400 * 1000
                && m.deltas > 0
            {
                advice.push("index is older than a week with deltas applied; `greeg index` rebuilds it with resolved imports and fresh ranks".into());
            }
            if !m.phase2 && !building {
                advice.push("phase 2 (symbols) never completed; run `greeg index`".into());
            }
            // freshness mode
            let fs_id = greeg_fsevents_id();
            let mode = if cfg!(target_os = "macos") && m.files >= 8000 && fs_id != 0 {
                "fsevents"
            } else {
                "stat"
            };
            writeln!(
                w,
                "freshness auto → {} ({} files{}; TTL 100 ms; last check {})",
                mode,
                fmt_n(m.files as usize),
                if cfg!(target_os = "macos") {
                    if fs_id != 0 {
                        ", FSEvents available"
                    } else {
                        ", FSEvents unavailable"
                    }
                } else {
                    ""
                },
                ago(m.verified_unix_ms)
            )?;
            // languages from the file table
            if let Ok(idx) = greeg_index::Index::open(&dir) {
                let mut per: BTreeMap<String, (usize, usize, bool)> = BTreeMap::new();
                for (_, rel, rec) in idx.live_files() {
                    let l = Lang::from_path(Path::new(rel));
                    let e = per
                        .entry(l.name().to_string())
                        .or_insert((0, 0, l.has_grammar()));
                    e.0 += 1;
                    if FileFlags(rec.flags).has(FileFlags::PARSE_ERRORS) {
                        e.1 += 1;
                    }
                }
                let mut v: Vec<_> = per.into_iter().collect();
                v.sort_by(|a, b| b.1.0.cmp(&a.1.0));
                let parts: Vec<String> = v
                    .iter()
                    .map(|(n, (files, errs, gram))| {
                        format!(
                            "{} {}{}",
                            n,
                            fmt_n(*files),
                            if *gram {
                                if *errs > 0 {
                                    format!(" (grammar ✓, {} fallbacks)", fmt_n(*errs))
                                } else {
                                    " (grammar ✓)".into()
                                }
                            } else {
                                String::new()
                            }
                        )
                    })
                    .collect();
                writeln!(w, "languages {}", parts.join(" · "))?;
            }
        }
    }
    // extra languages
    let reg = extra::registry();
    if reg.is_empty() {
        writeln!(
            w,
            "extra     none ({})",
            extra::lang_dir()
                .map(|d| d.display().to_string())
                .unwrap_or_default()
        )?;
    } else {
        let parts: Vec<String> = reg
            .iter()
            .enumerate()
            .map(|(i, l)| {
                format!(
                    "{} [{}] {}",
                    l.name,
                    l.extensions.join(","),
                    if sym::extra_ready(i as u8) {
                        "grammar ✓ query ✓"
                    } else {
                        "✗ (grammar or tags.scm unusable; run `greeg lang check`)"
                    }
                )
            })
            .collect();
        writeln!(w, "extra     {}", parts.join(" · "))?;
    }
    // sessions
    let sdir = dir.join("session");
    let (n, bytes) = std::fs::read_dir(&sdir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.metadata().ok())
                .fold((0usize, 0u64), |(n, b), m| (n + 1, b + m.len()))
        })
        .unwrap_or((0, 0));
    writeln!(
        w,
        "sessions  {} files ({}) in {}",
        n,
        fmt_size(bytes),
        sdir.display()
    )?;
    if let Some(m) = &manifest
        && m.files == 0
    {
        advice.push("the index has no files: is --root right, or is everything gitignored?".into());
    }
    for a in &advice {
        writeln!(w, "advice    {a}")?;
    }
    Ok(())
}

fn greeg_fsevents_id() -> u64 {
    #[cfg(target_os = "macos")]
    {
        greeg_fsevents::current_id()
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

/// `greeg lang check DIR`
pub fn lang_check(dir: &Path) -> Result<()> {
    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    let reg = extra::registry();
    writeln!(
        w,
        "language dir: {}",
        extra::lang_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_default()
    )?;
    if reg.is_empty() {
        writeln!(
            w,
            "no extra languages registered (each needs a subdirectory with spec.toml, grammar.so|dylib and tags.scm)"
        )?;
        return Ok(());
    }
    for (i, l) in reg.iter().enumerate() {
        let ready = sym::extra_ready(i as u8);
        writeln!(
            w,
            "\n{}  extensions [{}]  symbol {}  dir {}  {}",
            l.name,
            l.extensions.join(","),
            l.symbol,
            l.dir.display(),
            if ready {
                "grammar ✓ query ✓"
            } else {
                "✗ grammar or tags.scm unusable"
            }
        )?;
        if !ready {
            continue;
        }
        let lang = Lang::Extra(i as u8);
        let mut files = 0usize;
        let mut fallbacks = 0usize;
        let mut errors = 0usize;
        let mut kinds: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut noncode = 0usize;
        let mut imports = 0usize;
        let mut bytes = 0u64;
        let t0 = std::time::Instant::now();
        for e in ignore::WalkBuilder::new(dir).hidden(true).build().flatten() {
            if !e.file_type().map(|t| t.is_file()).unwrap_or(false)
                || Lang::from_path(e.path()) != lang
            {
                continue;
            }
            let Ok(src) = std::fs::read(e.path()) else {
                continue;
            };
            if src.len() > 4 << 20 || src[..src.len().min(8192)].contains(&0) {
                continue;
            }
            files += 1;
            bytes += src.len() as u64;
            let ex = sym::extract(lang, false, &src);
            if !ex.tree_sitter {
                fallbacks += 1;
            }
            if ex.parse_errors {
                errors += 1;
            }
            for s in &ex.symbols {
                *kinds.entry(s.kind.name()).or_default() += 1;
            }
            noncode += ex.noncode.len();
            imports += ex.imports.len();
            if files >= 5000 {
                break;
            }
        }
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        if files == 0 {
            writeln!(w, "  no {} files under {}", l.name, dir.display())?;
            continue;
        }
        let ks: Vec<String> = kinds
            .iter()
            .map(|(k, n)| format!("{k} {}", fmt_n(*n)))
            .collect();
        writeln!(
            w,
            "  {} files, {} · {:.0} ms ({:.1} MB/s) · parse errors {} · fallbacks {} · noncode spans {} · imports {}",
            fmt_n(files),
            fmt_size(bytes),
            ms,
            bytes as f64 / 1e6 / (ms / 1e3).max(1e-9),
            errors,
            fallbacks,
            fmt_n(noncode),
            fmt_n(imports)
        )?;
        writeln!(
            w,
            "  symbols: {}",
            if ks.is_empty() {
                "none (check the @def.* captures in tags.scm)".to_string()
            } else {
                ks.join(", ")
            }
        )?;
    }
    Ok(())
}
