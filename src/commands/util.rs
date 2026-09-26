// ---------------------------------------------------------------------------
// commands/util.rs — shared CLI-side plumbing: output mode selection, DB
// opening, interactive prompts, and the small formatting helpers every
// command leans on. Free of clap so handlers stay focused on their command.
// ---------------------------------------------------------------------------

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result};

use mdrv_oc as oc;

// ---------------------------------------------------------------------------
// output mode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Output {
    Human,
    Json,
    PrettyJson,
}

impl Output {
    pub(crate) fn from_flags(json: bool, pretty: bool, _quiet: bool) -> Self {
        if pretty {
            Output::PrettyJson
        } else if json {
            Output::Json
        } else {
            Output::Human
        }
    }

    pub(crate) fn is_json(&self) -> bool {
        matches!(self, Output::Json | Output::PrettyJson)
    }

    /// Print a `Serialize` value when in a JSON mode, otherwise run the closure
    /// for human output. Centralizing this keeps every command consistent.
    pub(crate) fn emit<T: serde::Serialize>(&self, value: &T, human: impl FnOnce()) {
        match self {
            Output::Json => println!("{}", serde_json::to_string(value).unwrap()),
            Output::PrettyJson => {
                println!("{}", serde_json::to_string_pretty(value).unwrap());
            }
            Output::Human => human(),
        }
    }
}

// ---------------------------------------------------------------------------
// DB open helper
// ---------------------------------------------------------------------------

/// Resolve the `--db` override (or the default location) and open the DB.
pub(crate) fn open_db(override_path: Option<&std::path::Path>) -> Result<oc::Db> {
    let db = match override_path {
        Some(p) => oc::Db::open(p).with_context(|| format!("opening {}", p.display()))?,
        None => oc::Db::open_default().context("opening default opencode.db")?,
    };
    Ok(db)
}

// ---------------------------------------------------------------------------
// interactive helpers
// ---------------------------------------------------------------------------

/// Print `prompt`, read one line, interpret it as the codebase's standard
/// `[y/N]` answer. EOF (closed stdin) counts as "no".
pub(crate) fn ask_yes(prompt: &str) -> Result<bool> {
    Ok(match prompt_line(prompt)? {
        Some(line) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        None => false,
    })
}

/// Print `prompt` and read one line from stdin; `None` on EOF.
pub(crate) fn prompt_line(prompt: &str) -> Result<Option<String>> {
    print!("{prompt}");
    io::stdout().flush().ok();
    let mut line = String::new();
    let n = io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading input")?;
    if n == 0 {
        return Ok(None);
    }
    Ok(Some(line))
}

// ---------------------------------------------------------------------------
// formatting helpers
// ---------------------------------------------------------------------------

/// `12 B` / `34.1 KB` / `5.6 MB` — compact size for listings.
pub(crate) fn fmt_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b < 1024.0 {
        format!("{bytes} B")
    } else if b < 1024.0 * 1024.0 {
        format!("{:.1} KB", b / 1024.0)
    } else {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    }
}

/// `950` / `68.5k` / `15.5M` — compact decimal counts (message and token
/// counts read better with 1000-based magnitudes than with KiB units).
pub(crate) fn fmt_count(n: i64) -> String {
    let a = n.unsigned_abs();
    if a < 1_000 {
        a.to_string()
    } else if a < 1_000_000 {
        format!("{:.1}k", a as f64 / 1_000.0)
    } else if a < 1_000_000_000 {
        format!("{:.1}M", a as f64 / 1_000_000.0)
    } else {
        format!("{:.1}B", a as f64 / 1_000_000_000.0)
    }
}

/// Last path segment, for compact listings.
pub(crate) fn file_label(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Stem of an export file name: `x.json.zst` / `x.json` → `x`.
pub(crate) fn export_stem_of(p: &std::path::Path) -> String {
    let name = file_label(p);
    name.strip_suffix(".json.zst")
        .or_else(|| name.strip_suffix(".json"))
        .unwrap_or(&name)
        .to_string()
}

pub(crate) fn print_session_human(s: &oc::Session) {
    println!("id        : {}", s.id);
    if let Some(pid) = &s.parent_id {
        println!("parent    : {pid}");
    }
    println!("project   : {}", s.project_id);
    println!("slug      : {}", s.slug);
    println!("title     : {}", s.title);
    println!("directory : {}", s.directory.display());
    println!("created   : {}", fmt_unix_ms(s.time_created));
    println!("updated   : {}", fmt_unix_ms(s.time_updated));
}

/// The `session show` extra lines (model, tokens, ...). `None` fields are
/// skipped — older schemas simply don't carry them.
pub(crate) fn print_details_human(d: &oc::session::SessionDetails) {
    if let Some(m) = &d.model {
        println!("model     : {m}");
    }
    if let Some(a) = &d.agent {
        println!("agent     : {a}");
    }
    if let Some(v) = &d.version {
        println!("oc version: {v}");
    }
    if let Some(c) = d.message_count {
        println!("messages  : {}", fmt_count(c));
    }
    if let Some(cost) = d.cost {
        println!("cost      : ${cost:.2}");
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = d.tokens_input {
        parts.push(format!("in {}", fmt_count(t)));
    }
    if let Some(t) = d.tokens_output {
        parts.push(format!("out {}", fmt_count(t)));
    }
    if let Some(t) = d.tokens_cache_read {
        parts.push(format!("cache read {}", fmt_count(t)));
    }
    if let Some(t) = d.tokens_cache_write {
        parts.push(format!("cache write {}", fmt_count(t)));
    }
    if let Some(t) = d.tokens_reasoning {
        parts.push(format!("reasoning {}", fmt_count(t)));
    }
    if !parts.is_empty() {
        println!("tokens    : {}", parts.join(", "));
    }
}

/// Render a Unix-millis timestamp as a stable, locale-independent string.
/// (Deliberately not full ISO-8601-with-timezone to avoid pulling in `chrono`
/// for the MVP; the raw epoch-ms is always visible in JSON mode anyway.)
pub(crate) fn fmt_unix_ms(ms: i64) -> String {
    let secs = ms / 1000;
    let days = secs / 86400;
    format!("{ms}ms ({secs}s / day {days} since epoch)")
}

/// Render a Unix-millis timestamp as `YYYY-MM-DD` (UTC) — used by the
/// interactive menu. Pure integer math (days-since-epoch → civil date,
/// Howard Hinnant's algorithm) so no `chrono` dependency is needed.
pub(crate) fn fmt_date(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Convert days since 1970-01-01 to (year, month, day). Proleptic Gregorian,
/// valid for any date expressible as an integer day count.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (y + i64::from(m <= 2), m as u32, d as u32)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_date_known_days() {
        // Fixtures computed independently (python datetime.date.toordinal).
        assert_eq!(fmt_date(0), "1970-01-01");
        assert_eq!(fmt_date(19_782 * 86_400_000), "2024-02-29"); // leap day
        assert_eq!(fmt_date(20_681 * 86_400_000), "2026-08-16");
        assert_eq!(fmt_date(11_017 * 86_400_000), "2000-03-01");
        assert_eq!(fmt_date(10_956 * 86_400_000), "1999-12-31");
    }

    #[test]
    fn fmt_date_truncates_to_day() {
        // Mid-day millis land on the same day as midnight.
        let midnight = 20_681 * 86_400_000;
        let midday = midnight + 12 * 3_600_000;
        assert_eq!(fmt_date(midnight), fmt_date(midday));
    }

    #[test]
    fn fmt_count_magnitudes() {
        assert_eq!(fmt_count(950), "950");
        assert_eq!(fmt_count(68_462), "68.5k");
        assert_eq!(fmt_count(15_513_920), "15.5M");
        assert_eq!(fmt_count(2_300_000_000), "2.3B");
        assert_eq!(fmt_count(0), "0");
    }
}
