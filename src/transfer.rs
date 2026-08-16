// ===========================================================================
// transfer.rs — bulk export/import of sessions via the `opencode` CLI.
//
// OpenCode v1.18 ships `opencode export <id>` (session JSON on stdout) and
// `opencode import <file>` — but only one session at a time. This module adds
// the bulk layer: loop the child process per session/file and add the small
// pure helpers the CLI needs (selection-spec parsing, import-file inspection,
// path expansion).
//
// Why shell out instead of writing rows ourselves? The export format is an
// OpenCode-internal schema that drifts between versions (and `import` also
// re-anchors the session to the current project — logic we do not want to
// duplicate). Delegating to the installed `opencode` binary keeps mdrv-oc
// byte-compatible with whatever version the user runs.
//
// When mdrv-oc is pointed at a non-default DB via `--db`, the same path is
// propagated to the child through `OPENCODE_DB` so both sides agree.
// ===========================================================================

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::{Error, Result};

/// zstd level for exports. 3 is libzstd's default: hundreds of MB/s on JSON,
/// typically 4-8x smaller. Exports are interactive-frequency events, so a
/// higher level would buy ~5% ratio for less headroom — not worth it.
const ZSTD_LEVEL: i32 = 3;

/// zstd frame magic (0xFD2FB528, little-endian on disk). Used to detect
/// compressed import files regardless of their extension.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

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

/// Export a single session by exact id: run `opencode export <id>` with its
/// stdout redirected **directly into** a file in `out_dir`, validate it, and
/// — unless `compress` is false — re-compress it to `<id>.json.zst`
/// (removing the intermediate plain file).
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
    out_dir: &Path,
    compress: bool,
) -> Result<ExportOutcome> {
    use std::process::Stdio;

    std::fs::create_dir_all(out_dir)?;
    let path = out_dir.join(format!("{id}.json"));
    let label = format!("{} export {id}", bin.display());

    let mut cmd = Command::new(bin);
    cmd.arg("export").arg(id);
    if let Some(db) = db_override {
        // Keep mdrv-oc and the child talking to the same database.
        cmd.env("OPENCODE_DB", db);
    }
    let file = std::fs::File::create(&path)?;
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
        cleanup_best_effort(&path);
        return Err(Error::External {
            cmd: label,
            detail: stderr_text.trim().to_string(),
        });
    }

    // Read back + sanity-check: must be a JSON object with an `info` object.
    // Catches half-written output without being so strict that future format
    // additions break us.
    let raw = std::fs::read(&path)?;
    let value: serde_json::Value = serde_json::from_slice(&raw).map_err(|e| {
        cleanup_best_effort(&path);
        Error::InvalidInput {
            msg: format!("`{} export {id}` wrote invalid JSON: {e}", bin.display()),
        }
    })?;
    if !value.is_object() || value.get("info").map(|i| i.is_object()) != Some(true) {
        cleanup_best_effort(&path);
        return Err(Error::InvalidInput {
            msg: format!(
                "`{} export {id}` output has no `info` object — unexpected format",
                bin.display()
            ),
        });
    }

    if compress {
        let zst = out_dir.join(format!("{id}.json.zst"));
        if let Err(e) = compress_file(&path, &zst) {
            cleanup_best_effort(&zst);
            cleanup_best_effort(&path);
            return Err(e);
        }
        std::fs::remove_file(&path)?;
        let bytes = std::fs::metadata(&zst)?.len();
        Ok(ExportOutcome {
            id: id.to_string(),
            bytes,
            raw_bytes: raw.len() as u64,
            path: zst,
            compressed: true,
        })
    } else {
        Ok(ExportOutcome {
            id: id.to_string(),
            bytes: raw.len() as u64,
            raw_bytes: raw.len() as u64,
            path,
            compressed: false,
        })
    }
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

/// What `inspect_import_file` extracted from a candidate import file.
#[derive(Debug, Clone, Serialize)]
pub struct ImportMeta {
    pub path: PathBuf,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Peek at an export file without touching the DB: parse it and pull
/// `info.id` / `info.title`. Transparently decompresses `.json.zst` files
/// (detected by magic bytes). Fails (InvalidInput) on unreadable files,
/// invalid JSON/zstd, or a missing `info.id` — the CLI skips those with a
/// warning.
pub fn inspect_import_file(path: &Path) -> Result<ImportMeta> {
    let raw: Vec<u8> = if peek_is_zst(path) {
        zstd::decode_all(std::fs::File::open(path)?).map_err(|e| Error::InvalidInput {
            msg: format!("{}: not valid zstd: {e}", path.display()),
        })?
    } else {
        std::fs::read(path)?
    };
    let value: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| Error::InvalidInput {
            msg: format!("{}: not valid JSON: {e}", path.display()),
        })?;
    let info = value
        .get("info")
        .and_then(|i| i.as_object())
        .ok_or_else(|| Error::InvalidInput {
            msg: format!(
                "{}: no `info` object — not an opencode export file?",
                path.display()
            ),
        })?;
    let session_id = info
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::InvalidInput {
            msg: format!("{}: `info.id` missing or not a string", path.display()),
        })?
        .to_string();
    let title = info.get("title").and_then(|v| v.as_str()).map(String::from);
    Ok(ImportMeta {
        path: path.to_path_buf(),
        session_id,
        title,
    })
}

/// Run `opencode import <file>` for one already-inspected file. Returns the
/// child's success line (e.g. `Imported session: ses_…`).
///
/// Compressed inputs (magic-byte detected) are decompressed to a temp file
/// first — `opencode import` expects plain JSON — and the temp file is
/// removed whether the import succeeds or not.
pub fn import_file(bin: &Path, db_override: Option<&Path>, file: &Path) -> Result<String> {
    if !peek_is_zst(file) {
        return spawn_import(bin, db_override, file);
    }
    let tmp = decompress_to_temp(file)?;
    let result = spawn_import(bin, db_override, &tmp);
    let _ = std::fs::remove_file(&tmp);
    result
}

fn spawn_import(bin: &Path, db_override: Option<&Path>, file: &Path) -> Result<String> {
    let file_str = file.to_string_lossy().into_owned();
    let mut cmd = Command::new(bin);
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

/// Expand the positional arguments of `session import`: a directory becomes
/// its `*.json` / `*.json.zst` children (sorted by name, non-recursive), a
/// file passes through, anything else is an error.
pub fn expand_import_paths(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for input in inputs {
        if input.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(input)?
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
            out.extend(entries);
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
    fn inspect_reads_id_and_title() {
        let dir = std::env::temp_dir().join("mdrv-oc-test-inspect");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("ok.json");
        std::fs::write(
            &f,
            r#"{"info":{"id":"ses_1","slug":"s","title":"Hello"},"messages":[]}"#,
        )
        .unwrap();
        let meta = inspect_import_file(&f).unwrap();
        assert_eq!(meta.session_id, "ses_1");
        assert_eq!(meta.title.as_deref(), Some("Hello"));
    }

    #[test]
    fn inspect_rejects_bad_files() {
        let dir = std::env::temp_dir().join("mdrv-oc-test-inspect");
        std::fs::create_dir_all(&dir).unwrap();
        let not_json = dir.join("not.json");
        std::fs::write(&not_json, "nope{").unwrap();
        assert!(inspect_import_file(&not_json).is_err());

        let no_id = dir.join("noid.json");
        std::fs::write(&no_id, r#"{"info":{"slug":"x"},"messages":[]}"#).unwrap();
        assert!(inspect_import_file(&no_id).is_err());
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

    #[test]
    fn zstd_roundtrip_detection_and_inspect() {
        let dir = std::env::temp_dir().join("mdrv-oc-test-zst");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Valid JSON with a long repetitive field: compresses hugely.
        let payload = format!(
            r#"{{"info":{{"id":"ses_1","title":"{}"}},"messages":[]}}"#,
            "x".repeat(20_000)
        );
        let json = dir.join("s.json");
        std::fs::write(&json, &payload).unwrap();

        let zst = dir.join("s.json.zst");
        compress_file(&json, &zst).unwrap();

        // tiny, detected, and lossless
        assert!(zst.metadata().unwrap().len() < payload.len() as u64 / 100);
        assert!(peek_is_zst(&zst));
        assert!(!peek_is_zst(&json));
        let back = zstd::decode_all(std::fs::File::open(&zst).unwrap()).unwrap();
        assert_eq!(back, payload.as_bytes());

        // inspect sees through the compression
        let meta = inspect_import_file(&zst).unwrap();
        assert_eq!(meta.session_id, "ses_1");
        assert_eq!(meta.title.as_deref(), Some("x".repeat(20_000).as_str()));
    }
}
