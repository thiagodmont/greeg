//! The output budget: `--budget` for one run, else `GREEG_BUDGET`, else
//! `budget` in the config file (`greeg budget LEVEL`), else [`DEFAULT`].

use crate::stats::{Source, config_path, read_config, write_config_key};
use anyhow::Result;
use std::sync::OnceLock;

pub const DEFAULT: usize = 2000;

/// Named budgets, in tokens; `none` is unlimited.
pub const LEVELS: [(&str, usize); 4] =
    [("low", 1000), ("medium", 2000), ("high", 5000), ("none", 0)];

/// A level name or a token count.
pub fn parse(s: &str) -> Result<usize, String> {
    let s = s.trim();
    if let Some((_, n)) = LEVELS.iter().find(|(name, _)| name.eq_ignore_ascii_case(s)) {
        return Ok(*n);
    }
    s.parse()
        .map_err(|_| format!("expected low, medium, high, none or a number of tokens, not {s:?}"))
}

/// `1000 (low)`, `0 (none: no limit)`, `3000`.
pub fn describe(n: usize) -> String {
    match LEVELS.iter().find(|(_, v)| *v == n) {
        Some(("none", _)) => "0 (none: no limit)".to_string(),
        Some((name, _)) => format!("{n} ({name})"),
        None => n.to_string(),
    }
}

/// The budget a run uses without `--budget`, and where it came from. An
/// unreadable value is reported once and ignored.
fn decide() -> (usize, Source) {
    if let Ok(v) = std::env::var("GREEG_BUDGET") {
        match parse(&v) {
            Ok(n) => return (n, Source::Env),
            Err(e) => eprintln!("greeg: ignoring GREEG_BUDGET: {e}"),
        }
    }
    match read_config().budget.as_deref().map(parse) {
        Some(Ok(n)) => (n, Source::Config),
        Some(Err(e)) => {
            eprintln!("greeg: ignoring `budget` in the config file: {e}");
            (DEFAULT, Source::Default)
        }
        None => (DEFAULT, Source::Default),
    }
}

/// [`decide`], once per process.
pub fn configured() -> usize {
    static B: OnceLock<usize> = OnceLock::new();
    *B.get_or_init(|| decide().0)
}

const NONE_WARNING: &str = "every search and verb now prints every match: a common word can put hundreds of thousands of tokens into an agent's context. `--budget N` still limits one run.";

/// `greeg budget [LEVEL]`: show the budget, or save one to the config file.
pub fn run(level: Option<&str>) -> Result<()> {
    let Some(level) = level else {
        let (n, source) = decide();
        let from = match source {
            Source::Env => "GREEG_BUDGET".to_string(),
            Source::Config => config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            Source::Default => "default".to_string(),
        };
        println!("budget {} · {from}", describe(n));
        println!(
            "levels: {}",
            LEVELS
                .iter()
                .map(|(name, v)| format!("{name} {v}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        return Ok(());
    };
    let n = parse(level).map_err(anyhow::Error::msg)?;
    let path = write_config_key("budget", &n.to_string())?;
    println!("budget {} · saved to {}", describe(n), path.display());
    if n == 0 {
        println!("{NONE_WARNING}");
    }
    if std::env::var_os("GREEG_BUDGET").is_some() {
        println!("GREEG_BUDGET is set in this shell and takes precedence");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_and_numbers_parse() {
        assert_eq!(parse("low"), Ok(1000));
        assert_eq!(parse("Medium"), Ok(2000));
        assert_eq!(parse("high"), Ok(5000));
        assert_eq!(parse("none"), Ok(0));
        assert_eq!(parse(" 3000 "), Ok(3000));
        assert!(parse("lots").is_err());
        assert!(parse("-1").is_err());
        assert_eq!(describe(1000), "1000 (low)");
        assert_eq!(describe(0), "0 (none: no limit)");
        assert_eq!(describe(3000), "3000");
    }
}
