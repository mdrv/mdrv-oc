// ===========================================================================
// transfer.rs — bulk export/import of sessions via the `opencode` CLI.
//
// OpenCode ships one-session-at-a-time transfer commands, but their location
// moved between major versions: v1.17–1.18 have top-level `opencode export
// <id>` / `opencode import <file>`, OpenCode v2 renamed them to `opencode
// session export <id>` / `opencode session import <file|URL>`. This module
// adds the bulk layer around whichever form the installed binary speaks
// (detected once via `--version`, see [`detect_flavor`]).
//
// Why shell out instead of writing rows ourselves? The export format is an
// OpenCode-internal schema that drifts between versions (and `import` also
// re-anchors the session to the current project — logic we do not want to
// duplicate). Delegating to the installed `opencode` binary keeps mdrv-oc
// byte-compatible with whatever version the user runs.
//
// On top of the child process, this module owns everything callers need to
// make export files *self-describing*:
//   - [`peek_export_file`] — read id/title/directory/dates out of an export
//     file (transparently decompressing zstd) while streaming only as far as
//     the `info` object;
//   - [`index_export_dir`] — index a whole directory by the session ids
//     found *inside* the files, so file names stay free-form;
//   - naming helpers ([`sanitize_name`], [`default_export_stem`],
//     [`unique_stem`]) and the newer-wins [`decide_export`] policy.
//
// When mdrv-oc is pointed at a non-default DB via `--db`, the same path is
// propagated to the child through `OPENCODE_DB` so both sides agree (v2 still
// honors it).
// ===========================================================================

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::model::Session;
use crate::{Error, Result};

/// zstd level for exports. 3 is libzstd's default: hundreds of MB/s on JSON,
/// typically 4-8x smaller. Exports are interactive-frequency events, so a
/// higher level would buy ~5% ratio for less headroom — not worth it.
const ZSTD_LEVEL: i32 = 3;

/// zstd frame magic (0xFD2FB528, little-endian on disk). Used to detect
/// compressed import files regardless of their extension.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

// ---------------------------------------------------------------------------
// opencode flavor (v1 vs v2 command forms)
// ---------------------------------------------------------------------------

/// Which invocation form the installed `opencode` binary speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenCodeFlavor {
    /// `opencode export <id>` / `opencode import <file>` (v1.17–1.18).
    V1,
    /// `opencode session export <id>` / `opencode session import <file>` (v2.x).
    V2,
}

/// Detect the installed binary's flavor by parsing `--version`. One cheap
/// child spawn per mdrv-oc command (not per session).
pub fn detect_flavor(bin: &Path) -> OpenCodeFlavor {
    let Ok(out) = Command::new(bin).arg("--version").output() else {
        // Can't ask — assume the current era; the real call will report
        // a precise error either way.
        return OpenCodeFlavor::V2;
    };
    flavor_from_version(&String::from_utf8_lossy(&out.stdout))
}

/// `"opencode v2.0.14"` → V2, `"1.18.5"` → V1, unparseable → V2.
fn flavor_from_version(s: &str) -> OpenCodeFlavor {
    let major = s.split_whitespace().find_map(|tok| {
        let digits = tok.trim_start_matches(|c: char| !c.is_ascii_digit());
        digits.split('.').next()?.parse::<u32>().ok()
    });
    match major {
        Some(m) if m < 2 => OpenCodeFlavor::V1,
        _ => OpenCodeFlavor::V2,
    }
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

/// One successful `opencode export <id>` → file on disk.
#[derive(Debug, Clone, Serialize)]
pub struct ExportOutcome {
    pub id: String,
    pub path: PathBuf,
    /// Size of the artifact on disk (the compressed size when `compressed`).
    pub bytes: u64,
    /// Uncompressed JSON size (equals `bytes` when not compressed).
    pub raw_bytes: u64,
    /// True when the artifact is a `.json.zst` file.
    pub compressed: bool,
}

/// Export a single session by exact id into the exact destination `dest`
/// (the CLI decides the file name — dated default stem or custom name — and
/// the newer-wins policy before calling).
///
/// The child's stdout is redirected **directly into** a scratch file next to
/// `dest` and — unless `compress` is false — re-compressed to `dest`
/// afterwards (the scratch file is always removed).
///
/// The file-redirect matters: `opencode export` truncates at exactly 128 KiB
/// (the pipe-buffer size) when its stdout is a pipe — observed on v1.18.18 —
/// but emits complete JSON when writing to a file. Writing the child's bytes
/// untouched also preserves whatever canonical form this version produces.
/// Compression happens *after* the child exits, as a plain file→file
/// `copy_encode`, so the workaround is never bypassed.
pub fn export_session(
    bin: &Path,
    db_override: Option<&Path>,
    id: &str,
    dest: &Path,
    compress: bool,
    flavor: OpenCodeFlavor,
) -> Result<ExportOutcome> {
    use std::process::Stdio;

    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    // Scratch name can never collide with a real export (those end in
    // `.json` / `.json.zst`) — overwriting a *different* session's file
    // here would destroy it.
    let raw = if compress {
        let stem = dest
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("export")
            .trim_end_matches(".json.zst")
            .trim_end_matches(".json");
        dest.with_file_name(format!("{stem}.tmp.json"))
    } else {
        dest.to_path_buf()
    };
    let label = format!("{} export {id}", bin.display());

    let mut cmd = Command::new(bin);
    if flavor == OpenCodeFlavor::V2 {
        cmd.arg("session");
    }
    cmd.arg("export").arg(id);
    if let Some(db) = db_override {
        // Keep mdrv-oc and the child talking to the same database.
        cmd.env("OPENCODE_DB", db);
    }
    let file = std::fs::File::create(&raw)?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(file.try_clone()?))
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| map_spawn_error(e, &label))?;
    drop(file); // close our handle before reading the file back

    // Drain stderr *before* wait() so a chatty child can't fill the pipe and
    // deadlock (error banners are tiny, but be safe).
    let mut stderr_text = String::new();
    if let Some(mut s) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut s, &mut stderr_text);
    }
    let status = child.wait()?;

    if !status.success() {
        cleanup_best_effort(&raw);
        return Err(Error::External {
            cmd: label,
            detail: stderr_text.trim().to_string(),
        });
    }

    // Read back + sanity-check: must be a JSON object with an `info` object.
    // Catches half-written output without being so strict that future format
    // additions break us.
    let raw_bytes = std::fs::read(&raw)?;
    let value: serde_json::Value = serde_json::from_slice(&raw_bytes).map_err(|e| {
        cleanup_best_effort(&raw);
        Error::InvalidInput {
            msg: format!("`{} export {id}` wrote invalid JSON: {e}", bin.display()),
        }
    })?;
    if !value.is_object() || value.get("info").map(|i| i.is_object()) != Some(true) {
        cleanup_best_effort(&raw);
        return Err(Error::InvalidInput {
            msg: format!(
                "`{} export {id}` output has no `info` object — unexpected format",
                bin.display()
            ),
        });
    }

    if compress {
        if let Err(e) = compress_file(&raw, dest) {
            cleanup_best_effort(dest);
            cleanup_best_effort(&raw);
            return Err(e);
        }
        std::fs::remove_file(&raw)?;
        let bytes = std::fs::metadata(dest)?.len();
        Ok(ExportOutcome {
            id: id.to_string(),
            bytes,
            raw_bytes: raw_bytes.len() as u64,
            path: dest.to_path_buf(),
            compressed: true,
        })
    } else {
        Ok(ExportOutcome {
            id: id.to_string(),
            bytes: raw_bytes.len() as u64,
            raw_bytes: raw_bytes.len() as u64,
            path: dest.to_path_buf(),
            compressed: false,
        })
    }
}

// ---------------------------------------------------------------------------
// peeking — what's inside an export file?
// ---------------------------------------------------------------------------

/// Metadata peeked from an export file. Handles both layouts: v1 keeps the
/// working directory in `info.directory`, v2 in `info.location.directory`
/// (with `slug`/`directory` nulled out).
#[derive(Debug, Clone, Serialize)]
pub struct ExportInfo {
    pub path: PathBuf,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// v1 `info.directory`, falling back to v2 `info.location.directory`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    /// Unix-millis, straight from `info.time`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_created: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_updated: Option<i64>,
    /// Size on disk (compressed size for `.json.zst` files).
    pub size_bytes: u64,
}

// serde view of the `info` object; `#[serde(default)]` tolerates absent and
// null fields alike (v2 nulls out `slug`/`directory`).
#[derive(Deserialize)]
struct RawInfo {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    location: Option<RawLocation>,
    #[serde(default)]
    time: Option<RawTime>,
}

#[derive(Deserialize)]
struct RawLocation {
    #[serde(default)]
    directory: Option<String>,
}

#[derive(Deserialize)]
struct RawTime {
    #[serde(default)]
    created: Option<i64>,
    #[serde(default)]
    updated: Option<i64>,
}

/// Peek at an export file without touching the DB or spawning `opencode`:
/// extract `info.id`, `info.title`, the working directory and the session's
/// timestamps. `.json.zst` files (magic-byte detected) are transparently
/// decompressed — and only as far as the `info` object, which exports place
/// before the (often huge) `messages` array.
///
/// Fails (InvalidInput) on unreadable files, invalid zstd/JSON, or a missing
/// `info.id` — callers skip those with a warning.
pub fn peek_export_file(path: &Path) -> Result<ExportInfo> {
    let size_bytes = std::fs::metadata(path)?.len();
    let bad = |msg: String| Error::InvalidInput {
        msg: format!("{}: {msg}", path.display()),
    };
    let info_bytes: Vec<u8> = if peek_is_zst(path) {
        let file = std::fs::File::open(path)?;
        let decoder = zstd::stream::Decoder::new(std::io::BufReader::new(file))
            .map_err(|e| bad(format!("not valid zstd: {e}")))?;
        scan_info_object(decoder).map_err(bad)?
    } else {
        let file = std::fs::File::open(path)?;
        scan_info_object(std::io::BufReader::new(file)).map_err(bad)?
    };
    let raw: RawInfo = serde_json::from_slice(&info_bytes)
        .map_err(|e| bad(format!("`info` is not a session info object: {e}")))?;
    let directory = raw.directory.filter(|d| !d.is_empty()).or_else(|| {
        raw.location
            .and_then(|l| l.directory)
            .filter(|d| !d.is_empty())
    });
    Ok(ExportInfo {
        path: path.to_path_buf(),
        session_id: raw.id,
        title: raw.title,
        slug: raw.slug,
        directory,
        time_created: raw.time.as_ref().and_then(|t| t.created),
        time_updated: raw.time.as_ref().and_then(|t| t.updated),
        size_bytes,
    })
}

/// Extract the raw bytes of the top-level `"info"` object's value from a JSON
/// stream, reading only as far as needed.
///
/// Hand-rolled byte scanner on purpose: serde_json cannot return a *partial*
/// top-level object, so `from_reader` would decompress the whole file before
/// yielding `info` — exactly the megabytes this exists to skip.
///
/// Supports both the minified and pretty-printed shapes OpenCode emits, and
/// correctly skips other values (including `messages` first) via full JSON
/// string/depth tracking. The `info` value must be an object; anything else
/// is an error string the caller turns into InvalidInput.
fn scan_info_object(read: impl std::io::Read) -> std::result::Result<Vec<u8>, String> {
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum St {
        /// Before the root `{`.
        PreRoot,
        /// Inside the root object, expecting a key string or the closing `}`.
        WantKey,
        /// Reading a root-level key string's bytes.
        InKey,
        /// Key string closed, expecting `:`.
        WantColon,
        /// `:` seen, expecting the value's first byte.
        WantValue,
        /// Skipping a root-level value that is not `info` (any JSON shape).
        Skipping,
        /// Inside the `info` object, collecting its bytes.
        Collecting,
    }

    let is_ws = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r');
    let truncated = || "no `info` object found (truncated or not an opencode export)".to_string();

    let mut st = St::PreRoot;
    let mut depth: usize = 0; // 0 = outside the root object
    let mut escaped = false; // the previous byte inside a string was `\`
    let mut key: Vec<u8> = Vec::new(); // bytes of the current root-level key
    let mut skip_open = 0u8; // Skipping: value opener ('{', '[', '"' or 0 = scalar)
    let mut skip_str = false; // Skipping: inside a (nested) string
    let mut col_str = false; // Collecting: inside a string of the info object
    let mut out = Vec::new();

    let mut buf = [0u8; 65_536];
    let mut read = std::io::BufReader::new(read);
    loop {
        let n = read
            .read(&mut buf)
            .map_err(|e| format!("reading JSON: {e}"))?;
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            match st {
                St::PreRoot => {
                    if is_ws(b) {
                        continue;
                    }
                    if b == b'{' {
                        depth = 1;
                        st = St::WantKey;
                    } else {
                        return Err("not a JSON object".to_string());
                    }
                }
                St::WantKey => {
                    // Accept the `,` separator that follows a skipped value.
                    if is_ws(b) || b == b',' {
                        continue;
                    }
                    if b == b'"' {
                        key.clear();
                        escaped = false;
                        st = St::InKey;
                    } else {
                        // closing `}` or garbage — root ended without info
                        return Err(truncated());
                    }
                }
                St::InKey => {
                    if escaped {
                        escaped = false;
                        key.push(b);
                    } else if b == b'\\' {
                        escaped = true;
                        key.push(b);
                    } else if b == b'"' {
                        st = St::WantColon;
                    } else {
                        key.push(b);
                    }
                }
                St::WantColon => {
                    if is_ws(b) {
                        continue;
                    }
                    if b == b':' {
                        st = St::WantValue;
                    } else {
                        return Err("malformed JSON: expected ':' after key".to_string());
                    }
                }
                St::WantValue => {
                    if is_ws(b) {
                        continue;
                    }
                    let target = key.as_slice() == b"info";
                    match b {
                        b'{' | b'[' => {
                            depth += 1;
                            skip_open = b;
                            skip_str = false;
                            if target {
                                out.push(b);
                                col_str = false;
                                escaped = false;
                                st = St::Collecting;
                            } else {
                                st = St::Skipping;
                            }
                        }
                        b'"' => {
                            skip_open = b'"';
                            skip_str = true;
                            escaped = false;
                            if target {
                                return Err("`info` is not an object".to_string());
                            }
                            st = St::Skipping;
                        }
                        _ => {
                            // scalar value (number / true / false / null)
                            skip_open = 0;
                            if target {
                                return Err("`info` is not an object".to_string());
                            }
                            st = St::Skipping;
                        }
                    }
                }
                St::Skipping => {
                    if skip_open == 0 {
                        // root-level scalar: ends at `,` (next key) or at the
                        // root's closing `}` — there is no deeper nesting here
                        if b == b',' {
                            st = St::WantKey;
                        } else if b == b'}' || b == b']' {
                            return Err(truncated());
                        }
                    } else if skip_open == b'"' {
                        // the whole skipped value is one string
                        if escaped {
                            escaped = false;
                        } else if b == b'\\' {
                            escaped = true;
                        } else if b == b'"' {
                            st = St::WantKey;
                        }
                    } else if skip_str {
                        // string nested inside the skipped object/array
                        if escaped {
                            escaped = false;
                        } else if b == b'\\' {
                            escaped = true;
                        } else if b == b'"' {
                            skip_str = false;
                        }
                    } else {
                        match b {
                            b'"' => skip_str = true,
                            b'{' | b'[' => depth += 1,
                            b'}' | b']' => {
                                depth -= 1;
                                if depth == 1 {
                                    st = St::WantKey;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                St::Collecting => {
                    out.push(b);
                    if col_str {
                        if escaped {
                            escaped = false;
                        } else if b == b'\\' {
                            escaped = true;
                        } else if b == b'"' {
                            col_str = false;
                        }
                    } else {
                        match b {
                            b'"' => col_str = true,
                            b'{' | b'[' => depth += 1,
                            b'}' | b']' => {
                                depth -= 1;
                                if depth == 1 {
                                    return Ok(out);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    Err(truncated())
}

// ---------------------------------------------------------------------------
// indexing an out directory by session id
// ---------------------------------------------------------------------------

/// Every export file found in one directory, indexed by the session id stored
/// *inside* each file — file names are free-form (dated defaults, custom
/// names, legacy `<id>.json.zst` all coexist).
#[derive(Debug, Default)]
pub struct ExportIndex {
    entries: Vec<ExportInfo>, // directory order (sorted by file name)
    by_id: HashMap<String, Vec<ExportInfo>>,
    /// Present-but-unreadable files (corrupt archives, non-export JSON).
    pub invalid: Vec<(PathBuf, String)>,
}

impl ExportIndex {
    /// Every indexed export file, in directory order.
    pub fn entries(&self) -> &[ExportInfo] {
        &self.entries
    }

    /// All copies of one session found in the directory (any file naming).
    pub fn all_for(&self, session_id: &str) -> &[ExportInfo] {
        self.by_id.get(session_id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The newest copy of a session by `time.updated`; ties resolve to the
    /// later file name (entries are directory-ordered), keeping the choice
    /// deterministic.
    pub fn newest_for(&self, session_id: &str) -> Option<&ExportInfo> {
        self.all_for(session_id)
            .iter()
            .max_by_key(|e| e.time_updated.unwrap_or(i64::MIN))
    }
}

/// Peek every `*.json` / `*.json.zst` file in `dir` (non-recursive, name-
/// sorted) and index them by the session id found inside. A missing directory
/// indexes as empty (fresh out dir). Files that fail to peek land in
/// [`ExportIndex::invalid`] instead of failing the whole scan.
pub fn index_export_dir(dir: &Path) -> Result<ExportIndex> {
    let mut idx = ExportIndex::default();
    if !dir.is_dir() {
        return Ok(idx);
    }
    for p in list_export_files(dir)? {
        match peek_export_file(&p) {
            Ok(info) => {
                idx.by_id
                    .entry(info.session_id.clone())
                    .or_default()
                    .push(info.clone());
                idx.entries.push(info);
            }
            Err(e) => idx.invalid.push((p, e.to_string())),
        }
    }
    Ok(idx)
}

/// Dedupe peeked files by session id — the newest copy wins (ties keep the
/// last file in the given order). Returns one entry per input: `None` when
/// this file *is* the kept copy, or `Some(winner_index)` of the copy that
/// supersedes it. Rows displayed to the user then number only the winners.
pub fn dedupe_newest(peeked: &[ExportInfo]) -> Vec<Option<usize>> {
    let mut best: HashMap<&str, usize> = HashMap::new(); // id -> winner index
    for (i, e) in peeked.iter().enumerate() {
        let winner = match best.get(e.session_id.as_str()) {
            Some(&k) => {
                // strictly newer wins; ties keep the later file
                let k_upd = peeked[k].time_updated.unwrap_or(i64::MIN);
                let i_upd = e.time_updated.unwrap_or(i64::MIN);
                if i_upd >= k_upd {
                    i
                } else {
                    k
                }
            }
            None => i,
        };
        best.insert(e.session_id.as_str(), winner);
    }
    peeked
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let winner = best[e.session_id.as_str()];
            if winner == i {
                None
            } else {
                Some(winner)
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// export file naming
// ---------------------------------------------------------------------------

/// Sanitize free-form user text into one filename component: trim, path
/// separators → `-`, whitespace runs → single `-`, control characters → `-`.
/// Returns `None` when nothing usable remains (empty, `.`, `..`).
pub fn sanitize_name(raw: &str) -> Option<String> {
    let mut out = String::new();
    for c in raw.trim().chars() {
        if c.is_whitespace() || c == '/' || c == '\\' || c.is_control() {
            out.push('-');
        } else {
            out.push(c);
        }
    }
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    if out.is_empty() || out == "." || out == ".." {
        return None;
    }
    Some(out)
}

/// Local-timezone `YYYY-MM-DD` (in `tz`) of a Unix-millis timestamp.
pub fn date_string_for(ms: i64, tz: &jiff::tz::TimeZone) -> Option<String> {
    let ts = jiff::Timestamp::from_millisecond(ms).ok()?;
    Some(ts.to_zoned(tz.clone()).date().to_string())
}

/// Default export file stem: `YYYY-MM-DD_<slug>_ses_<id8>` where the date is
/// the session start (`time_created`) in the *local* timezone. Fallbacks:
/// date created → updated → `undated`; name slug → sanitized title (first 40
/// chars) → `session`.
pub fn default_export_stem(s: &Session) -> String {
    let tz = jiff::tz::TimeZone::system();
    let date = date_string_for(s.time_created, &tz)
        .or_else(|| date_string_for(s.time_updated, &tz))
        .unwrap_or_else(|| "undated".to_string());
    stem_from_parts(&date, &s.slug, &s.title, &s.id)
}

/// Pure core of [`default_export_stem`] (testable with a fixed date string).
fn stem_from_parts(date: &str, slug: &str, title: &str, id: &str) -> String {
    let name = sanitize_name(slug)
        .or_else(|| sanitize_name(&title.chars().take(40).collect::<String>()))
        .unwrap_or_else(|| "session".to_string());
    let bare = id.strip_prefix("ses_").unwrap_or(id);
    let id8: String = bare.chars().take(8).collect();
    format!("{date}_{name}_ses_{id8}")
}

/// `stem.json[.zst]` — the on-disk name for a stem.
pub fn export_filename(stem: &str, compress: bool) -> String {
    if compress {
        format!("{stem}.json.zst")
    } else {
        format!("{stem}.json")
    }
}

/// First of `base`, `base-2`, `base-3`, … whose export file name is not
/// already in `taken`. Returns the chosen stem and the suffix number
/// (1 = no suffix was needed).
pub fn unique_stem(base: &str, compress: bool, taken: &HashSet<String>) -> (String, u32) {
    for n in 1u32.. {
        let stem = if n == 1 {
            base.to_string()
        } else {
            format!("{base}-{n}")
        };
        if !taken.contains(&export_filename(&stem, compress)) {
            return (stem, n);
        }
    }
    unreachable!("suffix loop always returns")
}

// ---------------------------------------------------------------------------
// newer-wins policy
// ---------------------------------------------------------------------------

/// What to do with a fresh export, given the newest copy already in the out
/// directory. Rule: **newer wins** (by `info.time.updated`), so a session
/// that kept growing re-exports over its stale copy while re-exporting
/// nothing new is a no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportDecision {
    /// No existing copy — write a new file.
    Write,
    /// An older copy of this session exists — overwrite that file in place.
    Overwrite(PathBuf),
    /// The existing copy is exactly as new — nothing to do.
    SkipUpToDate(PathBuf),
    /// The existing copy is *newer* than the session row — keep it.
    SkipStale(PathBuf),
}

/// Decide [`ExportDecision`] for a session whose DB row says `new_updated`
/// (None = unknown) against the newest existing copy (None = none found).
/// When either side's timestamp is unknown, the explicit export action wins
/// (Overwrite) rather than silently dropping fresh data.
pub fn decide_export(existing: Option<&ExportInfo>, new_updated: Option<i64>) -> ExportDecision {
    let Some(ex) = existing else {
        return ExportDecision::Write;
    };
    let path = ex.path.clone();
    match (ex.time_updated, new_updated) {
        (Some(old), Some(new)) if new > old => ExportDecision::Overwrite(path),
        (Some(old), Some(new)) if new == old => ExportDecision::SkipUpToDate(path),
        (Some(_), Some(_)) => ExportDecision::SkipStale(path),
        _ => ExportDecision::Overwrite(path),
    }
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

/// Run `opencode import <file>` for one already-peeked file. Returns the
/// child's success line (e.g. `Imported session: ses_…`).
///
/// Compressed inputs (magic-byte detected) are decompressed to a temp file
/// first — `opencode import` expects plain JSON — and the temp file is
/// removed whether the import succeeds or not.
pub fn import_file(
    bin: &Path,
    db_override: Option<&Path>,
    file: &Path,
    flavor: OpenCodeFlavor,
) -> Result<String> {
    if !peek_is_zst(file) {
        return spawn_import(bin, db_override, file, flavor);
    }
    let tmp = decompress_to_temp(file)?;
    let result = spawn_import(bin, db_override, &tmp, flavor);
    let _ = std::fs::remove_file(&tmp);
    result
}

fn spawn_import(
    bin: &Path,
    db_override: Option<&Path>,
    file: &Path,
    flavor: OpenCodeFlavor,
) -> Result<String> {
    let file_str = file.to_string_lossy().into_owned();
    let mut cmd = Command::new(bin);
    if flavor == OpenCodeFlavor::V2 {
        cmd.arg("session");
    }
    cmd.arg("import").arg(&file_str);
    if let Some(db) = db_override {
        // Keep mdrv-oc and the child talking to the same database.
        cmd.env("OPENCODE_DB", db);
    }
    let label = format!("{} import {file_str}", bin.display());
    let output = spawn_or_input_error(cmd, &label)?;
    if !output.status.success() {
        let mut detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout_tail = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if detail.is_empty() {
            detail = stdout_tail;
        } else if !stdout_tail.is_empty() {
            detail.push('\n');
            detail.push_str(&stdout_tail);
        }
        return Err(Error::External { cmd: label, detail });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Expand the positional arguments of `session import` / `session inspect`: a
/// directory becomes its `*.json` / `*.json.zst` children (sorted by name,
/// non-recursive), a file passes through, anything else is an error.
pub fn expand_import_paths(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for input in inputs {
        if input.is_dir() {
            out.extend(list_export_files(input)?);
        } else if input.is_file() {
            out.push(input.clone());
        } else {
            return Err(Error::NotFound {
                what: "import path",
                query: input.display().to_string(),
            });
        }
    }
    Ok(out)
}

/// Sorted, non-recursive `*.json` / `*.json.zst` children of a directory.
fn list_export_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.to_ascii_lowercase().ends_with(".json"))
                || p.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("zst"))
        })
        .collect();
    entries.sort();
    Ok(entries)
}

// ---------------------------------------------------------------------------
// interactive selection spec
// ---------------------------------------------------------------------------

/// Parse a multi-select answer like `"1,3-5"` (or `"all"`) against a list of
/// `max` numbered items into sorted, de-duplicated **1-based** indices.
///
/// Empty input → empty vec (the caller treats it as "selected nothing").
/// Any unparsable token → `InvalidInput` so the caller can re-prompt.
pub fn parse_selection_spec(input: &str, max: usize) -> Result<Vec<usize>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if matches!(trimmed.to_ascii_lowercase().as_str(), "all" | "a" | "*") {
        return Ok((1..=max).collect());
    }

    let mut picked: Vec<usize> = Vec::new();
    for token in trimmed.split(|c: char| c == ',' || c.is_whitespace()) {
        if token.is_empty() {
            continue;
        }
        let indices = parse_range_token(token, max)?;
        for i in indices {
            if !picked.contains(&i) {
                picked.push(i);
            }
        }
    }
    picked.sort_unstable();
    Ok(picked)
}

/// `N` → [N]; `N-M` → N..=M (either order); bounds-checked against 1..=max.
fn parse_range_token(token: &str, max: usize) -> Result<Vec<usize>> {
    let bad = || Error::InvalidInput {
        msg: format!("cannot parse selection {token:?} (expected e.g. 1,3-5,all)"),
    };
    let parsed = if let Some((a, b)) = token.split_once('-') {
        let a: usize = a.trim().parse().map_err(|_| bad())?;
        let b: usize = b.trim().parse().map_err(|_| bad())?;
        a.min(b)..=a.max(b)
    } else {
        let n: usize = token.trim().parse().map_err(|_| bad())?;
        n..=n
    };
    let mut out = Vec::new();
    for i in parsed {
        if i == 0 || i > max {
            return Err(Error::InvalidInput {
                msg: format!("selection {i} is out of range 1..={max}"),
            });
        }
        out.push(i);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// zstd plumbing
// ---------------------------------------------------------------------------

/// Compress `src` into `dst` with [`ZSTD_LEVEL`]. File→file streaming — the
/// whole artifact is never held in memory.
fn compress_file(src: &Path, dst: &Path) -> Result<()> {
    let input = std::fs::File::open(src)?;
    let output = std::fs::File::create(dst)?;
    zstd::stream::copy_encode(input, &output, ZSTD_LEVEL)?;
    Ok(())
}

/// True when `path` starts with the zstd frame magic. Cheaper and more robust
/// than trusting extensions (works for renamed files, rejects `foo.zst`
/// text files before libzstd has to).
fn peek_is_zst(path: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    std::io::Read::read_exact(&mut f, &mut magic).is_ok() && magic == ZSTD_MAGIC
}

/// Decompress a zstd file into `~tmp/mdrv-oc-import-<pid>-<name>` for the
/// duration of one child import. Unique per process; sequential imports in
/// one run reuse-and-overwrite safely.
fn decompress_to_temp(file: &Path) -> Result<PathBuf> {
    let stem = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".to_string());
    let stem = stem.strip_suffix(".zst").unwrap_or(&stem);
    let tmp = std::env::temp_dir().join(format!("mdrv-oc-import-{}-{stem}", std::process::id()));
    let input = std::fs::File::open(file)?;
    let output = std::fs::File::create(&tmp)?;
    zstd::stream::copy_decode(input, &output).map_err(|e| Error::InvalidInput {
        msg: format!("{}: not valid zstd: {e}", file.display()),
    })?;
    Ok(tmp)
}

// ---------------------------------------------------------------------------
// child-process plumbing
// ---------------------------------------------------------------------------

/// Map a `Command::spawn` failure to a friendly error (binary-not-found gets
/// its own install hint).
fn map_spawn_error(e: std::io::Error, label: &str) -> Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        Error::InvalidInput {
            msg: format!(
                "`opencode` binary not found ({label}) — install it or pass --opencode-bin"
            ),
        }
    } else {
        Error::External {
            cmd: label.to_string(),
            detail: e.to_string(),
        }
    }
}

/// Spawn a configured `Command` with piped streams, mapping spawn failures via
/// [`map_spawn_error`].
fn spawn_or_input_error(mut cmd: Command, label: &str) -> Result<std::process::Output> {
    cmd.stdin(std::process::Stdio::null());
    let output = cmd.output().map_err(|e| map_spawn_error(e, label))?;
    Ok(output)
}

/// Best-effort removal of a half-written export after a child failure —
/// leaving a truncated JSON file behind would be worse than nothing.
fn cleanup_best_effort(path: &Path) {
    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const V1_MINIFIED: &str = r#"{"info":{"id":"ses_v1","slug":"mighty-river","title":"T1","directory":"/x/g/a","time":{"created":1784985859180,"updated":1787023589832}},"messages":[{"info":{"role":"user"},"parts":[]}]}"#;

    const V2_PRETTY: &str = r#"{
  "info": {
    "id": "ses_v2",
    "projectID": "global",
    "slug": null,
    "directory": null,
    "title": "T2",
    "location": { "directory": "/x/g/b" },
    "model": { "id": "glm", "providerID": "p" },
    "time": { "created": 1789488121122, "updated": 1789569656855, "idle": 0, "viewed": 0 }
  },
  "messages": []
}"#;

    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("mdrv-oc-test-transfer");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn flavor_from_version_strings() {
        assert_eq!(flavor_from_version("opencode v2.0.14"), OpenCodeFlavor::V2);
        assert_eq!(flavor_from_version("2.0.0"), OpenCodeFlavor::V2);
        assert_eq!(flavor_from_version("1.18.5"), OpenCodeFlavor::V1);
        assert_eq!(flavor_from_version("opencode 1.18.18"), OpenCodeFlavor::V1);
        assert_eq!(flavor_from_version(""), OpenCodeFlavor::V2);
        assert_eq!(flavor_from_version("garbage"), OpenCodeFlavor::V2);
    }

    #[test]
    fn peek_reads_v1_layout() {
        let f = write_temp("v1.json", V1_MINIFIED);
        let info = peek_export_file(&f).unwrap();
        assert_eq!(info.session_id, "ses_v1");
        assert_eq!(info.title.as_deref(), Some("T1"));
        assert_eq!(info.slug.as_deref(), Some("mighty-river"));
        assert_eq!(info.directory.as_deref(), Some("/x/g/a"));
        assert_eq!(info.time_created, Some(1784985859180));
        assert_eq!(info.time_updated, Some(1787023589832));
        assert_eq!(info.size_bytes, V1_MINIFIED.len() as u64);
    }

    #[test]
    fn peek_reads_v2_layout() {
        let f = write_temp("v2.json", V2_PRETTY);
        let info = peek_export_file(&f).unwrap();
        assert_eq!(info.session_id, "ses_v2");
        assert_eq!(info.title.as_deref(), Some("T2"));
        assert_eq!(info.slug, None);
        // v2 moved the directory into location.directory
        assert_eq!(info.directory.as_deref(), Some("/x/g/b"));
        assert_eq!(info.time_created, Some(1789488121122));
        assert_eq!(info.time_updated, Some(1789569656855));
    }

    #[test]
    fn peek_scans_past_messages_first() {
        let f = write_temp(
            "messages-first.json",
            r#"{"messages":[1,2,{"nested":{"deep":[true,null,"str {x} [y]"]}}],"info":{"id":"ses_m","title":"M"}}"#,
        );
        let info = peek_export_file(&f).unwrap();
        assert_eq!(info.session_id, "ses_m");
        assert_eq!(info.title.as_deref(), Some("M"));
    }

    #[test]
    fn peek_ignores_decoys_in_strings() {
        // A *different* key holding a string that contains the literal bytes
        // `"info"` (escaped) plus braces/brackets must not trigger capture…
        let decoy = r#"{"note":"she said \"info\": {x} [y]","info":{"id":"ses_d","title":"ok"}}"#;
        let f = write_temp("decoy.json", decoy);
        assert_eq!(peek_export_file(&f).unwrap().session_id, "ses_d");

        // …and a near-miss key (`info2`) is not `info`.
        let near = r#"{"info2":{"id":"wrong"},"info":{"id":"ses_right"}}"#;
        let f = write_temp("near.json", near);
        assert_eq!(peek_export_file(&f).unwrap().session_id, "ses_right");

        // Braces/escapes inside the info object's own strings are collected
        // byte-exact.
        let tricky = r#"{"info":{"id":"ses_c","title":"a \"quoted\" {title} [br]"},"messages":[]}"#;
        let f = write_temp("tricky.json", tricky);
        let info = peek_export_file(&f).unwrap();
        assert_eq!(info.title.as_deref(), Some(r#"a "quoted" {title} [br]"#));
    }

    #[test]
    fn peek_rejects_bad_files() {
        for (name, body) in [
            ("not-json.json", "nope{"),
            ("no-info.json", r#"{"messages":[]}"#),
            ("no-id.json", r#"{"info":{"slug":"x"}}"#),
            ("truncated.json", r#"{"info":{"id":"x""#),
            ("info-null.json", r#"{"info":null,"messages":[]}"#),
        ] {
            let f = write_temp(name, body);
            assert!(peek_export_file(&f).is_err(), "{name} should be rejected");
        }
    }

    #[test]
    fn peek_reads_through_zstd() {
        // Big repetitive payload so the compressed artifact is realistically
        // tiny but the scanner still sees the full `info` object.
        let payload = format!(
            r#"{{"info":{{"id":"ses_z","title":"{}","directory":"/x/g/z","time":{{"created":1,"updated":2}}}},"messages":[]}}"#,
            "x".repeat(20_000)
        );
        let json = write_temp("z.json", &payload);
        let zst_path = json.with_extension("json.zst");
        compress_file(&json, &zst_path).unwrap();

        assert!(peek_is_zst(&zst_path));
        let info = peek_export_file(&zst_path).unwrap();
        assert_eq!(info.session_id, "ses_z");
        assert_eq!(info.directory.as_deref(), Some("/x/g/z"));
        assert_eq!(info.time_updated, Some(2));
        assert!(info.size_bytes < payload.len() as u64 / 10);
    }

    #[test]
    fn index_maps_ids_across_any_file_naming() {
        let dir = std::env::temp_dir().join("mdrv-oc-test-index");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mk = |name: &str, id: &str, updated: i64| {
            let raw =
                format!(r#"{{"info":{{"id":"{id}","time":{{"created":0,"updated":{updated}}}}}}}"#);
            let p = dir.join(name);
            if name.ends_with(".zst") {
                let tmp = dir.join(format!("{name}.tmp"));
                std::fs::write(&tmp, &raw).unwrap();
                compress_file(&tmp, &p).unwrap();
                std::fs::remove_file(&tmp).unwrap();
            } else {
                std::fs::write(&p, &raw).unwrap();
            }
        };
        // Same session, two copies with different names — old export style
        // vs. a custom name; the newer one must win.
        mk("ses_legacy.json.zst", "ses_1", 100);
        mk("my-custom-name.json", "ses_1", 200);
        mk("other.json.zst", "ses_2", 50);
        std::fs::write(dir.join("broken.json"), "garbage{").unwrap();
        std::fs::write(dir.join("ignore.md"), "not an export").unwrap();

        let idx = index_export_dir(&dir).unwrap();
        assert_eq!(idx.entries().len(), 3);
        assert_eq!(idx.invalid.len(), 1);
        assert_eq!(idx.all_for("ses_1").len(), 2);
        assert_eq!(idx.newest_for("ses_1").unwrap().time_updated, Some(200));
        assert_eq!(idx.newest_for("ses_2").unwrap().time_updated, Some(50));
        assert!(idx.all_for("ses_3").is_empty());
    }

    #[test]
    fn index_missing_dir_is_empty() {
        let idx = index_export_dir(Path::new("/nonexistent/mdrv-oc")).unwrap();
        assert!(idx.entries().is_empty());
        assert!(idx.invalid.is_empty());
    }

    /// A minimal in-memory [`ExportInfo`] for dedupe tests.
    fn info(id: &str, updated: i64) -> ExportInfo {
        ExportInfo {
            path: PathBuf::from(format!("{id}-{updated}.json.zst")),
            session_id: id.to_string(),
            title: None,
            slug: None,
            directory: None,
            time_created: None,
            time_updated: Some(updated),
            size_bytes: 0,
        }
    }

    #[test]
    fn dedupe_keeps_single_newest_per_session() {
        // One session in three files, equal timestamps: the last one wins.
        let equal = vec![info("ses_a", 100), info("ses_a", 100), info("ses_a", 100)];
        assert_eq!(dedupe_newest(&equal), vec![Some(2), Some(2), None]);

        // Older first, newer later; then a stale copy after the winner.
        let mixed = vec![info("ses_a", 100), info("ses_a", 300), info("ses_a", 200)];
        assert_eq!(dedupe_newest(&mixed), vec![Some(1), None, Some(1)]);

        // Distinct sessions never dedupe; unknown timestamps sort lowest.
        let ids = vec![info("ses_a", 7), info("ses_b", 7), {
            let mut e = info("ses_a", 7);
            e.time_updated = None;
            e
        }];
        assert_eq!(dedupe_newest(&ids), vec![None, None, Some(0)]);
    }

    #[test]
    fn sanitize_name_rules() {
        assert_eq!(
            sanitize_name("My Cool Guide!").as_deref(),
            Some("My-Cool-Guide!")
        );
        assert_eq!(sanitize_name("a/b\\c").as_deref(), Some("a-b-c"));
        assert_eq!(
            sanitize_name("  spaced   out  ").as_deref(),
            Some("spaced-out")
        );
        assert_eq!(sanitize_name("kebab-case").as_deref(), Some("kebab-case"));
        assert_eq!(sanitize_name("ünïcode_x").as_deref(), Some("ünïcode_x"));
        assert_eq!(sanitize_name("a\tb\nc").as_deref(), Some("a-b-c"));
        assert_eq!(sanitize_name(""), None);
        assert_eq!(sanitize_name("   "), None);
        assert_eq!(sanitize_name("."), None);
        assert_eq!(sanitize_name(".."), None);
    }

    #[test]
    fn stem_from_parts_defaults() {
        assert_eq!(
            stem_from_parts(
                "2026-09-24",
                "mighty-river",
                "Any Title",
                "ses_0668d4b94ffeXYZ"
            ),
            "2026-09-24_mighty-river_ses_0668d4b9"
        );
        // no slug → sanitized, truncated title (case is preserved)
        assert_eq!(
            stem_from_parts(
                "2026-09-24",
                "",
                "Implementasi awal chat app NIPBANG Nyaa!",
                "ses_abcdef1234"
            ),
            "2026-09-24_Implementasi-awal-chat-app-NIPBANG-Nyaa!_ses_abcdef12"
        );
        // neither slug nor title
        assert_eq!(
            stem_from_parts("2026-09-24", "", "", "ses_x"),
            "2026-09-24_session_ses_x"
        );
        // id without the ses_ prefix is used as-is
        assert_eq!(
            stem_from_parts("d", "s", "t", "abcdefghij"),
            "d_s_ses_abcdefgh"
        );
    }

    #[test]
    fn date_string_for_utc_matches_day_math() {
        let utc = jiff::tz::TimeZone::UTC;
        assert_eq!(date_string_for(0, &utc).unwrap(), "1970-01-01");
        assert_eq!(
            date_string_for(19_782 * 86_400_000, &utc).unwrap(),
            "2024-02-29"
        );
        assert!(date_string_for(i64::MIN, &utc).is_none()); // out of range
    }

    #[test]
    fn unique_stem_suffixes_on_collision() {
        let mut taken = HashSet::new();
        let (s, n) = unique_stem("a", true, &taken);
        assert_eq!((s.as_str(), n), ("a", 1));
        taken.insert("a.json.zst".to_string());
        let (s, n) = unique_stem("a", true, &taken);
        assert_eq!((s.as_str(), n), ("a-2", 2));
        taken.insert("a-2.json.zst".to_string());
        let (s, n) = unique_stem("a", true, &taken);
        assert_eq!((s.as_str(), n), ("a-3", 3));
        // plain .json collisions don't affect the .zst name space
        let mut taken = HashSet::new();
        taken.insert("a.json".to_string());
        let (s, n) = unique_stem("a", true, &taken);
        assert_eq!((s.as_str(), n), ("a", 1));
    }

    #[test]
    fn decide_export_newer_wins() {
        let mk = |updated: Option<i64>| ExportInfo {
            path: PathBuf::from("/out/existing.json.zst"),
            session_id: "ses_1".into(),
            title: None,
            slug: None,
            directory: None,
            time_created: None,
            time_updated: updated,
            size_bytes: 1,
        };

        assert_eq!(decide_export(None, Some(5)), ExportDecision::Write);

        let old = mk(Some(100));
        assert_eq!(
            decide_export(Some(&old), Some(200)),
            ExportDecision::Overwrite(old.path.clone())
        );
        assert_eq!(
            decide_export(Some(&old), Some(100)),
            ExportDecision::SkipUpToDate(old.path.clone())
        );
        assert_eq!(
            decide_export(Some(&old), Some(50)),
            ExportDecision::SkipStale(old.path.clone())
        );
        // unknown timestamps → the explicit export wins
        assert_eq!(
            decide_export(Some(&mk(None)), Some(200)),
            ExportDecision::Overwrite(old.path.clone())
        );
        assert_eq!(
            decide_export(Some(&old), None),
            ExportDecision::Overwrite(old.path)
        );
    }

    #[test]
    fn selection_spec_single_and_ranges() {
        assert_eq!(parse_selection_spec("1", 5).unwrap(), vec![1]);
        assert_eq!(parse_selection_spec("1,3", 5).unwrap(), vec![1, 3]);
        assert_eq!(parse_selection_spec("2-4", 5).unwrap(), vec![2, 3, 4]);
        // reversed range, spaces, duplicates
        assert_eq!(parse_selection_spec("4-2", 5).unwrap(), vec![2, 3, 4]);
        assert_eq!(parse_selection_spec(" 1, 3 ,1", 5).unwrap(), vec![1, 3]);
    }

    #[test]
    fn selection_spec_all_keyword() {
        assert_eq!(parse_selection_spec("all", 3).unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_selection_spec("A", 3).unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_selection_spec("*", 3).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn selection_spec_empty_means_none() {
        assert!(parse_selection_spec("", 5).unwrap().is_empty());
        assert!(parse_selection_spec("   ", 5).unwrap().is_empty());
    }

    #[test]
    fn selection_spec_rejects_garbage_and_out_of_range() {
        assert!(parse_selection_spec("x", 5).is_err());
        assert!(parse_selection_spec("1-", 5).is_err());
        assert!(parse_selection_spec("0", 5).is_err());
        assert!(parse_selection_spec("6", 5).is_err());
        assert!(parse_selection_spec("2-9", 5).is_err());
    }

    #[test]
    fn expand_directory_to_sorted_json_files() {
        let dir = std::env::temp_dir().join("mdrv-oc-test-expand");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.json"), "{}").unwrap();
        std::fs::write(dir.join("a.json"), "{}").unwrap();
        std::fs::write(dir.join("c.json.zst"), "{}").unwrap();
        std::fs::write(dir.join("ignore.txt"), "").unwrap();
        std::fs::write(dir.join("ignore.md"), "").unwrap();

        let single = vec![dir.join("a.json")];
        assert_eq!(
            expand_import_paths(&single).unwrap(),
            vec![dir.join("a.json")]
        );

        let expanded = expand_import_paths(std::slice::from_ref(&dir)).unwrap();
        assert_eq!(
            expanded,
            vec![
                dir.join("a.json"),
                dir.join("b.json"),
                dir.join("c.json.zst")
            ]
        );

        let missing = vec![dir.join("nope.json")];
        assert!(expand_import_paths(&missing).is_err());
    }
}
