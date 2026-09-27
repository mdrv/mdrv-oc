// ---------------------------------------------------------------------------
// commands/session.rs — `mdrv-oc session …` handlers: list, show, move,
// export, import, inspect, and the two-way `sync` loop.
//
// Handlers are glue: open the DB (or take one), call a `mdrv_oc` library
// function, print the result — plus the little bit of conversation
// (previews, confirmations, row selection) a device-to-device transfer tool
// needs to stay trustworthy.
// ---------------------------------------------------------------------------

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use mdrv_oc as oc;

use super::util::{
    ask_yes, export_stem_of, file_label, fmt_date, fmt_size, open_db, print_details_human,
    print_session_human, prompt_line, Output,
};

// ---------------------------------------------------------------------------
// list / show
// ---------------------------------------------------------------------------

pub(crate) fn cmd_session_list(db: &oc::Db, limit: u32, output: Output) -> Result<()> {
    let sessions = oc::session::list(db, limit).context("listing sessions")?;
    output.emit(&sessions, || {
        if sessions.is_empty() {
            println!("(no sessions)");
            return;
        }
        for s in &sessions {
            println!(
                "{id}  {slug}  {dir}  {title}",
                id = s.id,
                slug = s.slug,
                dir = s.directory.display(),
                title = s.title
            );
        }
    });
    Ok(())
}

pub(crate) fn cmd_session_show(db: &oc::Db, sel: &str, output: Output) -> Result<()> {
    let session = oc::session::resolve(db, &oc::SessionSelector::parse(sel))
        .with_context(|| format!("resolving session {sel:?}"))?;
    let details = oc::session::details(db, &session.id)
        .with_context(|| format!("reading details for {}", session.id))?;
    output.emit(
        &serde_json::json!({ "session": session, "details": details }),
        || {
            print_session_human(&session);
            print_details_human(&details);
        },
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// move
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_session_move(
    override_path: Option<&std::path::Path>,
    sel: &str,
    directory: Option<String>,
    project: Option<String>,
    auto_project: bool,
    children: bool,
    yes: bool,
    no_backup: bool,
    dry_run: bool,
    output: Output,
) -> Result<()> {
    if project.is_some() && auto_project {
        bail!("--project and --auto-project are mutually exclusive");
    }

    let mut db = open_db(override_path)?;
    let session = oc::session::resolve(&db, &oc::SessionSelector::parse(sel))
        .with_context(|| format!("resolving session {sel:?}"))?;

    // --- resolve the target directory: argument first, else prompt ---------
    let dir_input = match directory {
        Some(d) => d,
        None => {
            // Interactive prompt. Printed to stderr so stdout (and JSON output)
            // stays clean for piping.
            eprint!(
                "New directory for {id} ({title}):\n  > ",
                id = session.id,
                title = session.title
            );
            io::stderr().flush().ok();
            let mut line = String::new();
            let n = io::stdin()
                .lock()
                .read_line(&mut line)
                .context("reading directory from stdin")?;
            if n == 0 {
                bail!("no directory given and stdin is empty");
            }
            line
        }
    };
    let new_dir = oc::pathutil::normalize_directory(&dir_input)?;

    // --- resolve the target project (optional) -----------------------------
    let new_project_id: Option<String> = if let Some(pid) = project {
        // Validate the manually-supplied id exists.
        match oc::project::get(&db, &pid).with_context(|| format!("looking up project {pid:?}"))? {
            Some(_) => Some(pid),
            None => bail!("no project with id {pid:?}"),
        }
    } else if auto_project {
        match oc::project::find_by_directory(&db, &new_dir)? {
            Some(p) => {
                eprintln!(
                    "auto-project: matched {pid} ({worktree})",
                    pid = p.id,
                    worktree = p.worktree.display()
                );
                Some(p.id)
            }
            None => {
                eprintln!(
                    "auto-project: no project owns {}; directory only",
                    new_dir.display()
                );
                None
            }
        }
    } else {
        None
    };

    let n_children = oc::session::count_children(&db, &session.id)?;

    // --- present the plan --------------------------------------------------
    let plan = serde_json::json!({
        "session_id": session.id,
        "title": session.title,
        "slug": session.slug,
        "from": {
            "directory": session.directory,
            "project_id": session.project_id,
        },
        "to": {
            "directory": new_dir,
            "project_id": new_project_id,
        },
        "children": children,
        "child_count": n_children,
        "dry_run": dry_run,
    });

    match output {
        Output::Human => {
            println!("session  : {} ({})", session.id, session.title);
            println!(
                "directory: {}  ->  {}",
                session.directory.display(),
                new_dir.display()
            );
            match (&new_project_id, &session.project_id) {
                (Some(np), cur) => println!("project  : {}  ->  {}", cur, np),
                (None, _) => println!("project  : {}  (unchanged)", session.project_id),
            }
            if children {
                println!("children : rewrite {} child session(s)", n_children);
            }
        }
        other => other.emit(&plan, || {}),
    }

    if dry_run {
        if !output.is_json() {
            println!("\n--- dry run; re-run without --dry-run to apply ---");
        }
        return Ok(());
    }

    // --- confirm -----------------------------------------------------------
    if !yes && !output.is_json() {
        print!("apply this change? [y/N] ");
        io::stdout().flush().ok();
        let mut answer = String::new();
        io::stdin()
            .lock()
            .read_line(&mut answer)
            .context("reading confirmation")?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("aborted.");
            return Ok(());
        }
    }

    // --- backup ------------------------------------------------------------
    if !no_backup {
        backup_db(&db, !output.is_json())?;
    }

    // --- apply -------------------------------------------------------------
    let res = oc::session::session_move(
        &mut db,
        &session.id,
        &new_dir,
        new_project_id.as_deref(),
        children,
    )
    .context("updating session")?;

    match output {
        Output::Human => {
            if res.moved {
                println!("done     : {} updated.", res.id);
            } else {
                println!("no-op    : {} already had these values.", res.id);
            }
        }
        other => {
            other.emit(
                &serde_json::json!({ "result": res, "applied": res.moved }),
                || {},
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

/// `session export` — pick sessions (selectors → --filter → interactive menu),
/// then loop `opencode export <id>` writing one self-describing file per
/// session into --out: `YYYY-MM-DD_<slug>_ses_<id8>.json[.zst]` (local-time
/// session start, or a custom name via --name / the interactive prompt).
///
/// The out dir is indexed by the session ids found *inside* the existing
/// files, so re-exports converge (newer wins) and forgotten copies of the
/// same session — under any naming — are reported before writing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_session_export(
    override_path: Option<&std::path::Path>,
    selectors: Vec<String>,
    filter: Option<String>,
    limit: u32,
    out: Option<PathBuf>,
    compress: bool,
    bin: &std::path::Path,
    name: Option<String>,
    yes: bool,
    quiet: bool,
    output: Output,
) -> Result<()> {
    if !selectors.is_empty() && filter.is_some() {
        bail!("positional selectors and --filter are mutually exclusive");
    }
    // SQL `LIMIT 0` would mean "no rows"; treat 0 as "no cap".
    let cap = if limit == 0 { u32::MAX } else { limit };

    let db = open_db(override_path)?;

    // --- pick the sessions --------------------------------------------------
    let sessions: Vec<oc::Session> = if !selectors.is_empty() {
        let mut picked: Vec<oc::Session> = Vec::new();
        for sel in &selectors {
            let s = oc::session::resolve(&db, &oc::SessionSelector::parse(sel))
                .with_context(|| format!("resolving session {sel:?}"))?;
            if !picked.iter().any(|p| p.id == s.id) {
                picked.push(s);
            }
        }
        picked
    } else if let Some(text) = filter {
        if text.trim().is_empty() {
            bail!("--filter is empty");
        }
        oc::session::search(&db, &text, cap)?
    } else if output.is_json() || quiet {
        bail!(
            "no selectors or --filter given; the interactive menu needs a \
             terminal — pass selectors or --filter (especially with --json)"
        );
    } else {
        menu_pick_sessions(&db, cap)?
    };

    if sessions.is_empty() {
        if !output.is_json() {
            println!("(no sessions matched)");
        }
        return Ok(());
    }

    // --- custom naming ------------------------------------------------------
    // `--name` is a single-session flag; batch naming happens interactively.
    if name.is_some() && sessions.len() > 1 {
        bail!(
            "--name needs exactly one matching session ({} matched) — export \
             them one by one, or rerun without --name and answer the \
             interactive naming prompt",
            sessions.len()
        );
    }
    let single_name = match name.as_deref() {
        Some(n) => Some(
            oc::transfer::sanitize_name(n)
                .with_context(|| format!("--name {n:?} sanitizes to an empty file name"))?,
        ),
        None => None,
    };

    // `--out` wins over `$MDRV_OC_OUT`, which wins over the current directory.
    let out_src = out
        .or_else(|| std::env::var_os("MDRV_OC_OUT").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let out_dir = oc::pathutil::normalize_directory(&out_src.to_string_lossy())?;
    std::fs::create_dir_all(&out_dir)?;

    // --- index what the out dir already holds (by id inside the files) ------
    let index = oc::transfer::index_export_dir(&out_dir)?;
    if !quiet && !output.is_json() {
        for (path, why) in &index.invalid {
            eprintln!("note      : not indexed {}: {why}", path.display());
        }
    }
    // File names already claimed on disk — collision checks and `-2` suffixes.
    let mut taken: HashSet<String> = index
        .entries()
        .iter()
        .filter_map(|e| e.path.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .collect();

    // --- interactive per-session naming (opt-in, batch only) ----------------
    let interactive = sessions.len() > 1 && !yes && !output.is_json();
    let mut customs: Vec<Option<String>> = vec![None; sessions.len()];
    if interactive && ask_yes("custom file names? [y/N] ")? {
        for (i, s) in sessions.iter().enumerate() {
            let line = prompt_line(&format!(
                "  name for {} ({}) [enter = default]: ",
                s.slug, s.id
            ))?;
            customs[i] = line.as_deref().and_then(oc::transfer::sanitize_name);
        }
    }

    // --- confirm the batch --------------------------------------------------
    if interactive {
        if !ask_yes(&format!(
            "export {} session(s) to {}? [y/N] ",
            sessions.len(),
            out_dir.display()
        ))? {
            println!("aborted.");
            return Ok(());
        }
    } else if !quiet && !output.is_json() {
        println!(
            "exporting {} session(s) to {}",
            sessions.len(),
            out_dir.display()
        );
    }

    // --- run ----------------------------------------------------------------
    let flavor = oc::transfer::detect_flavor(bin);
    let mut exported: Vec<oc::transfer::ExportOutcome> = Vec::new();
    let mut skipped: Vec<serde_json::Value> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    for (i, s) in sessions.iter().enumerate() {
        // Remind the user when this session already has several copies here
        // (any naming) — a forgotten earlier export is easy to miss.
        let copies = index.all_for(&s.id);
        if copies.len() >= 2 && !quiet && !output.is_json() {
            let names: Vec<String> = copies
                .iter()
                .filter_map(|c| c.path.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .collect();
            println!(
                "! existing : {} already exported here {}x: {}",
                s.id,
                copies.len(),
                names.join(", ")
            );
        }

        // File name: explicit --name (single session) > interactive name >
        // dated default stem.
        let stem = if sessions.len() == 1 {
            single_name
                .clone()
                .unwrap_or_else(|| oc::transfer::default_export_stem(s))
        } else {
            customs[i]
                .clone()
                .unwrap_or_else(|| oc::transfer::default_export_stem(s))
        };

        let (dest, retire) = match export_destination(
            &index,
            s,
            &stem,
            compress,
            &out_dir,
            &mut taken,
            !quiet && !output.is_json(),
        ) {
            ExportDestination::Skip { file, reason } => {
                if !quiet && !output.is_json() {
                    println!(
                        "= up-to-date: {} — {} is already current",
                        s.id,
                        file_label(&file)
                    );
                }
                skipped.push(serde_json::json!({ "id": s.id, "file": file, "reason": reason }));
                continue;
            }
            ExportDestination::Stale { file } => {
                if !quiet && !output.is_json() {
                    println!(
                        "< skipped   : {} — {} is newer than the session row",
                        s.id,
                        file_label(&file)
                    );
                }
                skipped.push(serde_json::json!({
                    "id": s.id, "file": file, "reason": "existing-copy-newer"
                }));
                continue;
            }
            ExportDestination::Overwrite { dest, retire } => (dest, retire),
            ExportDestination::Write { dest } => (dest, None),
        };

        match oc::transfer::export_session(bin, override_path, &s.id, &dest, compress, flavor) {
            Ok(o) => {
                if let Some(old) = retire {
                    let _ = std::fs::remove_file(&old);
                }
                if !quiet && !output.is_json() {
                    if o.compressed {
                        println!(
                            "exported : {} -> {} ({:.1} KB -> {:.1} KB)",
                            o.id,
                            o.path.display(),
                            o.raw_bytes as f64 / 1024.0,
                            o.bytes as f64 / 1024.0
                        );
                    } else {
                        println!(
                            "exported : {} -> {} ({:.1} KB)",
                            o.id,
                            o.path.display(),
                            o.bytes as f64 / 1024.0
                        );
                    }
                }
                exported.push(o);
            }
            Err(e) => {
                eprintln!("failed   : {}: {e}", s.id);
                failed.push((s.id.clone(), e.to_string()));
            }
        }
    }

    if output.is_json() {
        let failed: Vec<serde_json::Value> = failed
            .into_iter()
            .map(|(id, error)| serde_json::json!({ "id": id, "error": error }))
            .collect();
        output.emit(
            &serde_json::json!({
                "out": out_dir,
                "opencode_flavor": if flavor == oc::transfer::OpenCodeFlavor::V2 { "v2" } else { "v1" },
                "exported": exported,
                "skipped": skipped,
                "failed": failed,
            }),
            || {},
        );
    } else if !failed.is_empty() {
        // successes were already printed per-item above
        bail!("{} of {} export(s) failed", failed.len(), sessions.len());
    } else if !quiet {
        println!(
            "done     : {} written, {} skipped, in {}",
            exported.len(),
            skipped.len(),
            out_dir.display()
        );
    }
    Ok(())
}

/// Outcome of newer-wins destination planning for one session.
enum ExportDestination {
    /// On-disk copy is already current (or the session row is behind it).
    Skip { file: PathBuf, reason: &'static str },
    /// On-disk copy is strictly newer than the DB row — keep it.
    Stale { file: PathBuf },
    /// Write here; remove `retire` (a format-flipped predecessor) afterwards.
    Overwrite {
        dest: PathBuf,
        retire: Option<PathBuf>,
    },
    /// Fresh file name.
    Write { dest: PathBuf },
}

/// Decide where one session's export lands, sharing the skip/overwrite/
/// unique-name logic between `export` and `sync`.
fn export_destination(
    index: &oc::transfer::ExportIndex,
    s: &oc::Session,
    stem: &str,
    compress: bool,
    out_dir: &std::path::Path,
    taken: &mut HashSet<String>,
    verbose: bool,
) -> ExportDestination {
    match oc::transfer::decide_export(index.newest_for(&s.id), Some(s.time_updated)) {
        oc::transfer::ExportDecision::SkipUpToDate(p) => ExportDestination::Skip {
            file: p,
            reason: "up-to-date",
        },
        oc::transfer::ExportDecision::SkipStale(p) => ExportDestination::Stale { file: p },
        oc::transfer::ExportDecision::Overwrite(old) => {
            // Same session, same stem, current format; when the format
            // flipped (--no-compress or back) the old artifact is retired
            // once the new one is on disk.
            let stem_old = export_stem_of(&old);
            let dest = out_dir.join(oc::transfer::export_filename(&stem_old, compress));
            let flipped = dest != old;
            let retire = flipped.then_some(old.clone());
            if verbose {
                println!(
                    "> overwrite : {} — {} is older{}",
                    s.id,
                    file_label(&old),
                    if flipped { " (format changed)" } else { "" }
                );
            }
            ExportDestination::Overwrite { dest, retire }
        }
        oc::transfer::ExportDecision::Write => {
            let (unique, n) = oc::transfer::unique_stem(stem, compress, taken);
            if n > 1 && verbose {
                println!("! renamed   : {stem} is claimed by another export — using {unique}");
            }
            let file_name = oc::transfer::export_filename(&unique, compress);
            taken.insert(file_name.clone());
            ExportDestination::Write {
                dest: out_dir.join(file_name),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

/// One row of the import preview: a peeked file plus its DB/duplicate status.
#[derive(Debug)]
struct ImportRow {
    info: oc::transfer::ExportInfo,
    status: RowStatus,
}

/// Why a row is (or is not) selected by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowStatus {
    /// Not in the DB — selected by default.
    New,
    /// In the DB, file strictly newer — selected by default (refresh).
    Refresh,
    /// In the DB, file not newer than the DB copy — skipped by default.
    UpToDate,
    /// In the DB, file strictly older — skipped by default.
    Stale,
    /// Another row (same session id, newer file) supersedes this one.
    Duplicate(usize), // 1-based row number of the kept copy
}

impl RowStatus {
    fn default_selected(self) -> bool {
        matches!(self, RowStatus::New | RowStatus::Refresh)
    }

    fn label(self) -> &'static str {
        match self {
            RowStatus::New => "[new]",
            RowStatus::Refresh => "[newer — refresh]",
            RowStatus::UpToDate => "[up-to-date]",
            RowStatus::Stale => "[older — skip]",
            RowStatus::Duplicate(_) => "[dup — skip]",
        }
    }

    fn key(self) -> &'static str {
        match self {
            RowStatus::New => "new",
            RowStatus::Refresh => "refresh",
            RowStatus::UpToDate => "up-to-date",
            RowStatus::Stale => "stale",
            RowStatus::Duplicate(_) => "duplicate",
        }
    }
}

/// Upstream v2 import can exit 0 without writing anywhere when a global
/// `opencode serve --service` daemon is running — the service imports into
/// its own database instead of the `--db` target. Our DB handle sees the
/// truth, so verify and warn; this is the one upstream lie we catch free.
fn verify_landed(db: &oc::Db, session_id: &str, human: bool) {
    if !human {
        return;
    }
    let landed = oc::session::exists(db, session_id).unwrap_or(true);
    if !landed {
        eprintln!(
            "warning  : {session_id} is NOT in {} after import — is an `opencode serve --service` daemon running? It may have imported elsewhere (see README)",
            db.path().display()
        );
    }
}

/// Peek every candidate path, reporting unreadable ones on stderr and
/// collecting them separately.
fn peek_all(paths: &[PathBuf]) -> (Vec<oc::transfer::ExportInfo>, Vec<PathBuf>) {
    let mut peeked = Vec::new();
    let mut invalid = Vec::new();
    for p in paths {
        match oc::transfer::peek_export_file(p) {
            Ok(i) => peeked.push(i),
            Err(e) => {
                eprintln!("skipping {}: {e}", p.display());
                invalid.push(p.clone());
            }
        }
    }
    (peeked, invalid)
}

/// DB-vs-file freshness for a peeked file that is its session's kept copy.
fn db_row_status(db: &oc::Db, e: &oc::transfer::ExportInfo) -> Result<RowStatus> {
    let exists = oc::session::exists(db, &e.session_id)
        .with_context(|| format!("checking for session {}", e.session_id))?;
    if !exists {
        return Ok(RowStatus::New);
    }
    let db_updated = oc::session::resolve(db, &oc::SessionSelector::parse(&e.session_id))
        .with_context(|| format!("loading session {}", e.session_id))?
        .time_updated;
    Ok(match e.time_updated {
        Some(f) if f > db_updated => RowStatus::Refresh,
        Some(f) if f == db_updated => RowStatus::UpToDate,
        Some(_) => RowStatus::Stale,
        None => RowStatus::UpToDate, // file age unknown — don't clobber by default
    })
}

/// Peek → dedupe to the newest file per session → DB statuses. Shared by
/// `import` and `sync`; rows keep file order, duplicates point at winners.
fn build_rows(db: &oc::Db, peeked: &[oc::transfer::ExportInfo]) -> Result<Vec<ImportRow>> {
    // superseded[i]: None if peeked[i] is its session's kept copy, else the
    // index of the copy that supersedes it.
    let superseded = oc::transfer::dedupe_newest(peeked);
    // Winners get consecutive row numbers in file order; duplicates later
    // point at their winner's row.
    let mut row_of: HashMap<usize, usize> = HashMap::new();
    let mut next_row = 1;
    for (i, s) in superseded.iter().enumerate() {
        if s.is_none() {
            row_of.insert(i, next_row);
            next_row += 1;
        }
    }

    let mut rows = Vec::with_capacity(peeked.len());
    for (i, e) in peeked.iter().enumerate() {
        let status = match superseded[i] {
            None => db_row_status(db, e)?,
            Some(k) => RowStatus::Duplicate(row_of[&k]),
        };
        rows.push(ImportRow {
            info: e.clone(),
            status,
        });
    }
    Ok(rows)
}

/// The plan/preview rows shared by `import` and `sync`.
fn row_plan(rows: &[ImportRow]) -> Vec<serde_json::Value> {
    rows.iter()
        .enumerate()
        .map(|(r, row)| {
            serde_json::json!({
                "row": r + 1,
                "file": row.info.path,
                "session_id": row.info.session_id,
                "title": row.info.title,
                "directory": row.info.directory,
                "time_created": row.info.time_created,
                "time_updated": row.info.time_updated,
                "status": row.status.key(),
                "default_selected": row.status.default_selected(),
            })
        })
        .collect()
}

/// The numbered preview line shared by `import` and `sync`.
fn print_row_line(r: usize, row: &ImportRow) {
    let date = row
        .info
        .time_created
        .or(row.info.time_updated)
        .map(fmt_date)
        .unwrap_or_else(|| "?".to_string());
    println!(
        "  {n:>3}) {date}  {id}  {title}  {dir}  {status} <- {file}",
        n = r + 1,
        date = date,
        id = row.info.session_id,
        title = row.info.title.as_deref().unwrap_or(""),
        dir = row.info.directory.as_deref().unwrap_or(""),
        status = row.status.label(),
        file = file_label(&row.info.path),
    );
}

/// Checkpoint + copy the DB to `<path>.bak` — the safety net every
/// DB-mutating command lays before touching anything.
fn backup_db(db: &oc::Db, human: bool) -> Result<()> {
    db.checkpoint().context("WAL checkpoint before backup")?;
    let bak = PathBuf::from(format!("{}.bak", db.path().display()));
    db.backup(&bak)
        .with_context(|| format!("backing up to {}", bak.display()))?;
    if human {
        eprintln!("backup   : {}", bak.display());
    }
    Ok(())
}

/// `session import` — peek export files, dedupe same-session files to the
/// newest, flag sessions already in the DB (stale re-imports are skipped by
/// default), let the user pick rows, back up, then loop
/// `opencode import <file>`. Invalid files are skipped with a warning.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_session_import(
    override_path: Option<&std::path::Path>,
    files: Vec<PathBuf>,
    into: Option<PathBuf>,
    yes: bool,
    no_backup: bool,
    bin: &std::path::Path,
    quiet: bool,
    output: Output,
) -> Result<()> {
    if files.is_empty() {
        bail!("no files given");
    }
    let paths = oc::transfer::expand_import_paths(&files).context("expanding import paths")?;
    if paths.is_empty() {
        bail!("no *.json / *.json.zst files found in the given path(s)");
    }

    let (peeked, invalid) = peek_all(&paths);
    if peeked.is_empty() {
        bail!("no importable export files among the given path(s)");
    }

    let db = open_db(override_path)?;
    let rows = build_rows(&db, &peeked)?;

    // Target working directory for the child `opencode import` — the session
    // is re-anchored to this directory's project instead of the cwd's.
    let cwd = match &into {
        Some(d) => Some(
            oc::pathutil::normalize_directory(&d.to_string_lossy())
                .with_context(|| format!("normalizing --into {}", d.display()))?,
        ),
        None => None,
    };

    // --- present the plan ---------------------------------------------------
    let plan = row_plan(&rows);
    match output {
        Output::Human => {
            println!("import {} session(s):", rows.len());
            for (r, row) in rows.iter().enumerate() {
                print_row_line(r, row);
            }
            if let Some(d) = &cwd {
                println!("into     : {}", d.display());
            }
            if !invalid.is_empty() {
                println!("  ({} invalid file(s) skipped)", invalid.len());
            }
        }
        other => other.emit(
            &serde_json::json!({ "plan": plan, "into": cwd, "invalid": invalid }),
            || {},
        ),
    }

    // --- select what to import ----------------------------------------------
    // Default set: new sessions + files newer than their DB copy. Older
    // copies, up-to-date copies and in-dir duplicates stay selectable by
    // row number.
    let default_rows: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.status.default_selected())
        .map(|(i, _)| i + 1)
        .collect();
    let chosen: Vec<usize> = if yes || output.is_json() {
        default_rows.clone()
    } else {
        loop {
            let line = prompt_line(&format!(
                "import which? [y] recommended ({}), [all] everything ({}), [enter] cancel, or 1,3-5: ",
                default_rows.len(),
                rows.len()
            ))?;
            let Some(line) = line else {
                println!("aborted.");
                return Ok(());
            };
            let t = line.trim();
            if t.is_empty() || t.eq_ignore_ascii_case("n") || t.eq_ignore_ascii_case("no") {
                println!("aborted.");
                return Ok(());
            }
            let lower = t.to_ascii_lowercase();
            if matches!(lower.as_str(), "y" | "yes") {
                break default_rows.clone();
            }
            // Anything else goes through the standard selection spec
            // (which also covers all / a / *).
            match oc::transfer::parse_selection_spec(t, rows.len()) {
                Ok(v) => break v,
                Err(e) => println!("  ({e})"),
            }
        }
    };

    if chosen.is_empty() {
        if !output.is_json() {
            println!("nothing selected.");
        }
        return Ok(());
    }

    // --- backup (import mutates the DB via the child process) ---------------
    if !no_backup {
        backup_db(&db, !output.is_json())?;
    }

    // --- run ----------------------------------------------------------------
    let flavor = oc::transfer::detect_flavor(bin);
    let mut imported: Vec<(String, PathBuf)> = Vec::new();
    let mut failed: Vec<(PathBuf, String)> = Vec::new();
    for r in &chosen {
        let row = &rows[r - 1];
        match oc::transfer::import_file(bin, override_path, &row.info.path, flavor, cwd.as_deref())
        {
            Ok(msg) => {
                let line = if msg.is_empty() {
                    row.info.session_id.clone()
                } else {
                    msg
                };
                if !quiet && !output.is_json() {
                    println!("ok       : {line} <- {}", row.info.path.display());
                }
                verify_landed(&db, &row.info.session_id, !quiet && !output.is_json());
                imported.push((row.info.session_id.clone(), row.info.path.clone()));
            }
            Err(e) => {
                eprintln!("failed   : {}: {e}", row.info.path.display());
                failed.push((row.info.path.clone(), e.to_string()));
            }
        }
    }

    if output.is_json() {
        let imported: Vec<serde_json::Value> = imported
            .into_iter()
            .map(|(id, path)| serde_json::json!({ "id": id, "file": path }))
            .collect();
        let failed: Vec<serde_json::Value> = failed
            .into_iter()
            .map(|(path, error)| serde_json::json!({ "file": path, "error": error }))
            .collect();
        let not_selected: Vec<serde_json::Value> = rows
            .iter()
            .enumerate()
            .filter(|(r, _)| !chosen.contains(&(r + 1)))
            .map(|(r, row)| {
                serde_json::json!({
                    "row": r + 1,
                    "file": row.info.path,
                    "session_id": row.info.session_id,
                    "reason": row.status.key(),
                })
            })
            .collect();
        output.emit(
            &serde_json::json!({
                "imported": imported,
                "failed": failed,
                "invalid_skipped": invalid,
                "not_selected": not_selected,
            }),
            || {},
        );
    } else if !failed.is_empty() {
        bail!("{} of {} import(s) failed", failed.len(), chosen.len());
    } else if !quiet {
        println!(
            "done     : {} imported, {} failed, {} not selected, {} invalid file(s) skipped",
            imported.len(),
            failed.len(),
            rows.len() - chosen.len(),
            invalid.len()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// inspect
// ---------------------------------------------------------------------------

/// `session inspect` — peek inside export file(s)/dir(s) without importing:
/// session id, title, working directory, start date, size, and whether the
/// session already exists in the local DB. No DB writes, no `opencode`
/// subprocess; handles both the v1 and the v2 export layout.
pub(crate) fn cmd_session_inspect(
    override_path: Option<&std::path::Path>,
    paths: Vec<PathBuf>,
    output: Output,
) -> Result<()> {
    if paths.is_empty() {
        bail!("no files given");
    }
    let expanded = oc::transfer::expand_import_paths(&paths).context("expanding paths")?;
    if expanded.is_empty() {
        bail!("no *.json / *.json.zst files found in the given path(s)");
    }

    let (infos, invalid) = peek_all(&expanded);
    if infos.is_empty() {
        bail!("no readable export files among the given path(s)");
    }

    let db = open_db(override_path)?;
    let mut in_db: HashMap<&str, bool> = HashMap::new();
    for e in &infos {
        in_db
            .entry(e.session_id.as_str())
            .or_insert_with(|| oc::session::exists(&db, &e.session_id).unwrap_or(false));
    }

    match output {
        Output::Human => {
            for e in &infos {
                let date = e
                    .time_created
                    .or(e.time_updated)
                    .map(fmt_date)
                    .unwrap_or_else(|| "?".to_string());
                println!(
                    "{date}  {id}  {title}  {dir}  ({size}){db_mark}  <- {file}",
                    date = date,
                    id = e.session_id,
                    title = e.title.as_deref().unwrap_or(""),
                    dir = e.directory.as_deref().unwrap_or(""),
                    size = fmt_size(e.size_bytes),
                    db_mark = if in_db[e.session_id.as_str()] {
                        "  [in db]"
                    } else {
                        ""
                    },
                    file = file_label(&e.path),
                );
            }
            if !invalid.is_empty() {
                println!("({} invalid file(s) skipped)", invalid.len());
            }
        }
        other => {
            let sessions: Vec<serde_json::Value> = infos
                .iter()
                .map(|e| {
                    let mut v = serde_json::to_value(e).unwrap();
                    v["in_db"] = serde_json::json!(in_db[e.session_id.as_str()]);
                    v
                })
                .collect();
            other.emit(
                &serde_json::json!({ "sessions": sessions, "invalid": invalid }),
                || {},
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// sync
// ---------------------------------------------------------------------------

/// One planned push (export) for the sync preview.
struct PushPlan {
    session_id: String,
    dest: PathBuf,
    /// `+` for a fresh file, `> old-name` for an overwrite.
    action: String,
    /// A format-flipped predecessor to remove after the write lands.
    retire: Option<PathBuf>,
}

/// `session sync` — the whole two-device loop against one synced directory
/// in a single command: pull (import files newer than their DB copy), then
/// push (export sessions newer than their on-disk copy). Everything
/// up-to-date is skipped; the preview lists both directions first.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_session_sync(
    override_path: Option<&std::path::Path>,
    dir: PathBuf,
    filter: Option<String>,
    existing_only: bool,
    compress: bool,
    yes: bool,
    no_backup: bool,
    bin: &std::path::Path,
    quiet: bool,
    output: Output,
) -> Result<()> {
    if !dir.is_dir() {
        bail!(
            "sync dir {} does not exist (create it, or check the path)",
            dir.display()
        );
    }
    let out_dir = oc::pathutil::normalize_directory(&dir.to_string_lossy())?;
    if let Some(text) = &filter {
        if text.trim().is_empty() {
            bail!("--filter is empty");
        }
    }

    let db = open_db(override_path)?;

    // --- pull plan: files in the dir vs the DB ------------------------------
    let paths = oc::transfer::expand_import_paths(std::slice::from_ref(&dir))
        .context("expanding sync dir")?;
    let (peeked, invalid) = peek_all(&paths);
    let pull_rows = if peeked.is_empty() {
        Vec::new()
    } else {
        build_rows(&db, &peeked)?
    };
    let pull: Vec<&ImportRow> = pull_rows
        .iter()
        .filter(|r| r.status.default_selected())
        .collect();

    // --- push plan: sessions vs what the dir already holds ------------------
    let index = oc::transfer::index_export_dir(&out_dir)?;
    if !quiet && !output.is_json() {
        for (path, why) in &index.invalid {
            eprintln!("note      : not indexed {}: {why}", path.display());
        }
    }
    let mut taken: HashSet<String> = index
        .entries()
        .iter()
        .filter_map(|e| e.path.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .collect();

    let mut sessions: Vec<oc::Session> = match &filter {
        Some(text) => oc::session::search(&db, text, u32::MAX)?,
        None => oc::session::list(&db, u32::MAX)?,
    };
    // --existing: bound the push side to sessions this dir already holds, so
    // syncing a device's copy never spawns exports for the whole database
    // (the pull side is inherently limited to the dir's contents either way).
    if existing_only {
        sessions.retain(|s| !index.all_for(&s.id).is_empty());
    }

    let mut push: Vec<PushPlan> = Vec::new();
    for s in &sessions {
        // Forgotten-copy reminder — same spirit as `export`.
        let copies = index.all_for(&s.id);
        if copies.len() >= 2 && !quiet && !output.is_json() {
            let names: Vec<String> = copies
                .iter()
                .filter_map(|c| c.path.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .collect();
            println!(
                "! existing : {} already exported here {}x: {}",
                s.id,
                copies.len(),
                names.join(", ")
            );
        }

        let stem = oc::transfer::default_export_stem(s);
        match export_destination(
            &index,
            s,
            &stem,
            compress,
            &out_dir,
            &mut taken,
            !quiet && !output.is_json(),
        ) {
            ExportDestination::Skip { .. } | ExportDestination::Stale { .. } => {}
            ExportDestination::Overwrite { dest, retire } => push.push(PushPlan {
                session_id: s.id.clone(),
                dest,
                action: String::from(">"),
                retire,
            }),
            ExportDestination::Write { dest } => push.push(PushPlan {
                session_id: s.id.clone(),
                dest,
                action: String::from("+"),
                retire: None,
            }),
        }
    }

    // --- preview ------------------------------------------------------------
    let pull_plan = row_plan(&pull_rows);
    if output.is_json() {
        output.emit(
            &serde_json::json!({
                "dir": out_dir,
                "considered": sessions.len(),
                "pull": pull_plan,
                "push": push.iter().map(|p| serde_json::json!({
                    "id": p.session_id,
                    "file": p.dest,
                    "action": p.action,
                })).collect::<Vec<_>>(),
                "push_skipped": sessions.len() - push.len(),
                "invalid": invalid,
            }),
            || {},
        );
    } else {
        println!(
            "sync with {} ({} session(s) considered)",
            out_dir.display(),
            sessions.len()
        );
        println!("pull (import from dir):");
        if pull.is_empty() {
            println!("  (nothing to pull)");
        }
        for (r, row) in pull_rows.iter().enumerate() {
            if row.status.default_selected() {
                print_row_line(r, row);
            }
        }
        println!("push (export to dir):");
        if push.is_empty() {
            println!("  (nothing to push)");
        }
        for p in &push {
            println!(
                "  {action}  {file}  {id}",
                action = p.action,
                file = file_label(&p.dest),
                id = p.session_id
            );
        }
        let skipped = sessions.len() - push.len();
        if skipped > 0 {
            println!("  ({skipped} already up-to-date / skipped)");
        }
        if !invalid.is_empty() {
            println!("  ({} invalid file(s) skipped)", invalid.len());
        }
    }

    if pull.is_empty() && push.is_empty() {
        if !output.is_json() {
            println!("nothing to sync.");
        }
        return Ok(());
    }

    // --- confirm ------------------------------------------------------------
    if !yes && !output.is_json() && !ask_yes("run this sync? [y/N] ")? {
        println!("aborted.");
        return Ok(());
    }

    // --- backup only when the DB is about to change (the pull) --------------
    if !pull.is_empty() && !no_backup {
        backup_db(&db, !output.is_json())?;
    }

    // --- run: pull first (bring remote changes home), then push -------------
    let flavor = oc::transfer::detect_flavor(bin);
    let mut pulled: Vec<(String, PathBuf)> = Vec::new();
    let mut pushed: Vec<(String, PathBuf)> = Vec::new();
    let mut failed: Vec<String> = Vec::new();

    for row in &pull {
        match oc::transfer::import_file(bin, override_path, &row.info.path, flavor, None) {
            Ok(msg) => {
                let line = if msg.is_empty() {
                    row.info.session_id.clone()
                } else {
                    msg
                };
                if !quiet && !output.is_json() {
                    println!("ok       : {line} <- {}", row.info.path.display());
                }
                verify_landed(&db, &row.info.session_id, !quiet && !output.is_json());
                pulled.push((row.info.session_id.clone(), row.info.path.clone()));
            }
            Err(e) => {
                eprintln!("failed   : {}: {e}", row.info.path.display());
                failed.push(row.info.session_id.clone());
            }
        }
    }

    for p in &push {
        match oc::transfer::export_session(
            bin,
            override_path,
            &p.session_id,
            &p.dest,
            compress,
            flavor,
        ) {
            Ok(o) => {
                if let Some(old) = &p.retire {
                    let _ = std::fs::remove_file(old);
                }
                if !quiet && !output.is_json() {
                    println!("exported : {} -> {}", o.id, o.path.display());
                }
                pushed.push((o.id.clone(), o.path.clone()));
            }
            Err(e) => {
                eprintln!("failed   : {}: {e}", p.session_id);
                failed.push(p.session_id.clone());
            }
        }
    }

    if output.is_json() {
        output.emit(
            &serde_json::json!({
                "pulled": pulled.iter().map(|(id, path)| serde_json::json!({
                    "id": id, "file": path
                })).collect::<Vec<_>>(),
                "pushed": pushed.iter().map(|(id, path)| serde_json::json!({
                    "id": id, "file": path
                })).collect::<Vec<_>>(),
                "failed": failed,
                "invalid_skipped": invalid,
            }),
            || {},
        );
    } else if !failed.is_empty() {
        bail!(
            "{} of {} sync operation(s) failed",
            failed.len(),
            pulled.len() + pushed.len()
        );
    } else if !quiet {
        println!(
            "done     : {} pulled, {} pushed, {} skipped",
            pulled.len(),
            pushed.len(),
            sessions.len() - push.len()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// interactive menu
// ---------------------------------------------------------------------------

/// Interactive multi-select: print a numbered session list, then loop until the
/// user gives a parsable spec ("1,3-5", "all", or empty to cancel). Returns the
/// chosen sessions in list order.
fn menu_pick_sessions(db: &oc::Db, cap: u32) -> Result<Vec<oc::Session>> {
    let candidates = oc::session::list(db, cap).context("listing sessions")?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    println!("sessions (newest first):");
    for (i, s) in candidates.iter().enumerate() {
        println!(
            "{n:>3}) {date}  {id}  {slug}  {title}",
            n = i + 1,
            date = fmt_date(s.time_updated),
            id = s.id,
            slug = s.slug,
            title = s.title
        );
    }

    let picked = loop {
        print!("export which? (e.g. 1,3-5 or all; enter to cancel): ");
        io::stdout().flush().ok();
        let mut line = String::new();
        let n = io::stdin()
            .lock()
            .read_line(&mut line)
            .context("reading selection")?;
        if n == 0 {
            bail!("no selection read (stdin closed)");
        }
        match oc::transfer::parse_selection_spec(&line, candidates.len()) {
            Ok(v) => break v,
            Err(e) => println!("  ({e})"),
        }
    };

    if picked.is_empty() {
        println!("no sessions selected.");
        return Ok(Vec::new());
    }
    Ok(picked
        .into_iter()
        .filter_map(|i| candidates.get(i - 1).cloned())
        .collect())
}
