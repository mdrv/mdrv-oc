// ===========================================================================
// cli.rs — the `mdrv-oc` command-line interface.
//
// Part of the *binary*, not the library: it lives in `src/` but is only pulled
// in by `src/main.rs` (`mod cli;`). That keeps `clap` out of the reusable core
// so the library stays embeddable.
//
// Structure mirrors `mdrv-ink`: a `Cli` root with global flags, a `Command`
// enum of subcommands, a small `Output` (Human/Json/PrettyJson) helper, and one
// handler function per command. Handlers are glue: open the DB → call a library
// function → print the result.
// ===========================================================================

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use mdrv_oc as oc;

// ---------------------------------------------------------------------------
// The command tree
// ---------------------------------------------------------------------------

/// `mdrv-oc` — inspect and manipulate the OpenCode SQLite database.
#[derive(Parser, Debug)]
#[command(
    name = "mdrv-oc",
    version,
    about = "Inspect and manipulate the OpenCode SQLite database",
    long_about = "Inspect and manipulate the OpenCode SQLite database.\n\
                  Move sessions between directories, or export/import them in \
                  bulk for device-to-device transfer. See README.md for background."
)]
pub struct Cli {
    /// Emit machine-readable JSON instead of human text.
    #[arg(short, long, global = true)]
    pub json: bool,

    /// Indented, pretty JSON (implies --json).
    #[arg(long, global = true)]
    pub pretty: bool,

    /// Suppress non-result commentary (progress, banners). JSON is unaffected.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Path to `opencode.db`. Defaults to the auto-detected OpenCode data dir.
    #[arg(long, global = true, value_name = "PATH")]
    pub db: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Inspect or modify sessions.
    Session {
        #[command(subcommand)]
        action: SessionCommand,
    },

    /// Inspect projects.
    Project {
        #[command(subcommand)]
        action: ProjectCommand,
    },

    /// Print the carapace completion spec for this binary.
    ///
    /// Install it with:
    ///   mdrv-oc completion > ~/.config/carapace/specs/mdrv-oc.yaml
    Completion,

    /// Internal: emit dynamic completion candidates for the carapace spec.
    #[command(hide = true, name = "__complete")]
    Complete {
        /// What to complete: "sessions" or "projects".
        what: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum SessionCommand {
    /// List sessions, newest first.
    List {
        /// How many sessions to show (default 20).
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },

    /// Show one session (exact id, id prefix, slug, or title substring).
    Show {
        /// Session selector: id, id prefix, slug, or title substring.
        session: String,
    },

    /// Change a session's directory (and optionally its project).
    #[command(visible_alias = "mv")]
    Move {
        /// Session selector: id, id prefix, slug, or title substring.
        session: String,

        /// New directory. If omitted, you are prompted for it interactively.
        directory: Option<String>,

        /// Also set `session.project_id` to this exact project id.
        #[arg(long, value_name = "ID")]
        project: Option<String>,

        /// Infer `project_id` from the new directory (worktree / link table).
        #[arg(long)]
        auto_project: bool,

        /// Also rewrite child sessions (`parent_id` = this session).
        #[arg(long)]
        children: bool,

        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,

        /// Skip the automatic `<db>.bak` backup.
        #[arg(long)]
        no_backup: bool,

        /// Print the planned change without writing.
        #[arg(long)]
        dry_run: bool,
    },

    /// Export session(s) to portable JSON files via `opencode export`.
    ///
    /// Pick sessions with selectors (id/prefix/slug/title), `--filter` (title
    /// or slug substring), or — with neither — an interactive multi-select
    /// menu. One `<session-id>.json.zst` file is written per session into
    /// --out (plain `.json` with --no-compress).
    Export {
        /// Session selector(s): id, id prefix, slug, or title substring.
        selectors: Vec<String>,

        /// Export every session whose slug/title contains TEXT (non-interactive).
        #[arg(long, value_name = "TEXT")]
        filter: Option<String>,

        /// Cap how many sessions the menu / --filter considers (0 = no cap).
        #[arg(long, default_value_t = 20)]
        limit: u32,

        /// Output directory for the export files (created if missing).
        #[arg(long, default_value = ".")]
        out: PathBuf,

        /// Write plain `<id>.json` instead of zstd-compressed `<id>.json.zst`.
        #[arg(long)]
        no_compress: bool,

        /// Path to the opencode binary to drive.
        #[arg(long, default_value = "opencode")]
        opencode_bin: PathBuf,

        /// Skip the bulk confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Import session export file(s) via `opencode import`.
    ///
    /// Takes one or more export files or directories (a directory imports
    /// every `*.json` / `*.json.zst` inside it). Plain `.json.zst` files
    /// (magic-byte detected) are decompressed transparently. The imported
    /// session keeps its id, so `opencode -s <id>` resumes it; like upstream
    /// import, it is re-anchored to the project of the current working
    /// directory.
    Import {
        /// Export file(s) or directory(ies) containing export files.
        files: Vec<PathBuf>,

        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,

        /// Skip the automatic `<db>.bak` backup.
        #[arg(long)]
        no_backup: bool,

        /// Path to the opencode binary to drive.
        #[arg(long, default_value = "opencode")]
        opencode_bin: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
pub enum ProjectCommand {
    /// List all known projects.
    List,

    /// Show one project by exact id.
    Show { id: String },
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// The single entry point called by `main`. Returning `anyhow::Result<()>`
/// means an `Err(e)` bubbles up and `main` prints it + exits non-zero.
pub fn run(cli: Cli) -> Result<()> {
    let output = Output::from_flags(cli.json, cli.pretty, cli.quiet);

    match cli.command {
        Command::Session { action } => match action {
            SessionCommand::List { limit } => {
                let db = open_db(cli.db.as_deref())?;
                cmd_session_list(&db, limit, output)?
            }
            SessionCommand::Show { session } => {
                let db = open_db(cli.db.as_deref())?;
                cmd_session_show(&db, &session, output)?
            }
            SessionCommand::Move {
                session,
                directory,
                project,
                auto_project,
                children,
                yes,
                no_backup,
                dry_run,
            } => cmd_session_move(
                cli.db.as_deref(),
                &session,
                directory,
                project,
                auto_project,
                children,
                yes,
                no_backup,
                dry_run,
                output,
            )?,
            SessionCommand::Export {
                selectors,
                filter,
                limit,
                out,
                no_compress,
                opencode_bin,
                yes,
            } => cmd_session_export(
                cli.db.as_deref(),
                selectors,
                filter,
                limit,
                out,
                !no_compress,
                &opencode_bin,
                yes,
                cli.quiet,
                output,
            )?,
            SessionCommand::Import {
                files,
                yes,
                no_backup,
                opencode_bin,
            } => cmd_session_import(
                cli.db.as_deref(),
                files,
                yes,
                no_backup,
                &opencode_bin,
                cli.quiet,
                output,
            )?,
        },
        Command::Project { action } => match action {
            ProjectCommand::List => {
                let db = open_db(cli.db.as_deref())?;
                cmd_project_list(&db, output)?
            }
            ProjectCommand::Show { id } => {
                let db = open_db(cli.db.as_deref())?;
                cmd_project_show(&db, &id, output)?
            }
        },
        Command::Completion => {
            // Baked at compile time so the spec always matches this binary.
            print!("{}", include_str!("../specs/mdrv-oc.yaml"));
        }
        Command::Complete { what } => cmd_complete(cli.db.as_deref(), &what)?,
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Output mode helper
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Output {
    Human,
    Json,
    PrettyJson,
}

impl Output {
    fn from_flags(json: bool, pretty: bool, _quiet: bool) -> Self {
        if pretty {
            Output::PrettyJson
        } else if json {
            Output::Json
        } else {
            Output::Human
        }
    }

    fn is_json(&self) -> bool {
        matches!(self, Output::Json | Output::PrettyJson)
    }

    /// Print a `Serialize` value when in a JSON mode, otherwise run the closure
    /// for human output. Centralizing this keeps every command consistent.
    fn emit<T: serde::Serialize>(&self, value: &T, human: impl FnOnce()) {
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
fn open_db(override_path: Option<&std::path::Path>) -> Result<oc::Db> {
    let db = match override_path {
        Some(p) => oc::Db::open(p).with_context(|| format!("opening {}", p.display()))?,
        None => oc::Db::open_default().context("opening default opencode.db")?,
    };
    Ok(db)
}

// ---------------------------------------------------------------------------
// completion helpers
// ---------------------------------------------------------------------------

/// Hidden `__complete` handler — prints `value\tdescription` candidates for the
/// carapace `$()` exec macro (see specs/mdrv-oc.yaml). Runs on every TAB, so
/// it stays fast (single SQLite query) and fails *silently* (a missing DB
/// completes to nothing rather than surfacing a red error candidate).
fn cmd_complete(override_path: Option<&std::path::Path>, what: &str) -> Result<()> {
    if !matches!(what, "sessions" | "projects") {
        return Ok(());
    }
    let db = match open_db(override_path) {
        Ok(db) => db,
        Err(_) => return Ok(()),
    };
    match what {
        "sessions" => {
            for s in oc::session::list(&db, 500)? {
                println!(
                    "{}\t{}",
                    s.id,
                    sanitize_desc(&format!("{} — {}", s.slug, s.title))
                );
            }
        }
        "projects" => {
            for p in oc::project::list(&db)? {
                println!(
                    "{}\t{}",
                    p.id,
                    sanitize_desc(&p.worktree.display().to_string())
                );
            }
        }
        _ => {}
    }
    Ok(())
}

/// Collapse tabs/newlines in a candidate description — carapace parses the
/// first tab as the value/description separator, so stray tabs would corrupt
/// the candidate list.
fn sanitize_desc(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\t' | '\n' | '\r' => ' ',
            _ => c,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// session handlers
// ---------------------------------------------------------------------------

fn cmd_session_list(db: &oc::Db, limit: u32, output: Output) -> Result<()> {
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

fn cmd_session_show(db: &oc::Db, sel: &str, output: Output) -> Result<()> {
    let session = oc::session::resolve(db, &oc::SessionSelector::parse(sel))
        .with_context(|| format!("resolving session {sel:?}"))?;
    output.emit(&session, || print_session_human(&session));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_session_move(
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
        // Checkpoint so the file copy includes everything committed in WAL.
        db.checkpoint().context("WAL checkpoint before backup")?;
        let bak = PathBuf::from(format!("{}.bak", db.path().display()));
        db.backup(&bak)
            .with_context(|| format!("backing up to {}", bak.display()))?;
        if !output.is_json() {
            eprintln!("backup   : {}", bak.display());
        }
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
// session export / import handlers
// ---------------------------------------------------------------------------

/// `session export` — pick sessions (selectors → --filter → interactive menu),
/// then loop `opencode export <id>` writing one `<id>.json.zst` per session
/// (plain `<id>.json` when compression is disabled).
#[allow(clippy::too_many_arguments)]
fn cmd_session_export(
    override_path: Option<&std::path::Path>,
    selectors: Vec<String>,
    filter: Option<String>,
    limit: u32,
    out: PathBuf,
    compress: bool,
    bin: &std::path::Path,
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

    let out_dir = oc::pathutil::normalize_directory(&out.to_string_lossy())?;

    // --- confirm the batch --------------------------------------------------
    if sessions.len() > 1 && !yes && !output.is_json() {
        print!(
            "export {} session(s) to {}? [y/N] ",
            sessions.len(),
            out_dir.display()
        );
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
    } else if !quiet && !output.is_json() {
        println!(
            "exporting {} session(s) to {}",
            sessions.len(),
            out_dir.display()
        );
    }

    // --- run ----------------------------------------------------------------
    let mut exported: Vec<oc::transfer::ExportOutcome> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    for s in &sessions {
        match oc::transfer::export_session(bin, override_path, &s.id, &out_dir, compress) {
            Ok(o) => {
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
            &serde_json::json!({ "out": out_dir, "exported": exported, "failed": failed }),
            || {},
        );
    } else if !failed.is_empty() {
        // successes were already printed per-item above
        bail!("{} of {} export(s) failed", failed.len(), sessions.len());
    } else if !quiet {
        println!(
            "done     : {} file(s) in {}",
            exported.len(),
            out_dir.display()
        );
    }
    Ok(())
}

/// `session import` — inspect export files, confirm, back up, then loop
/// `opencode import <file>`. Invalid files are skipped with a warning.
fn cmd_session_import(
    override_path: Option<&std::path::Path>,
    files: Vec<PathBuf>,
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

    // --- inspect every candidate file --------------------------------------
    let mut metas = Vec::new();
    let mut invalid: Vec<PathBuf> = Vec::new();
    for p in &paths {
        match oc::transfer::inspect_import_file(p) {
            Ok(m) => metas.push(m),
            Err(e) => {
                eprintln!("skipping {}: {e}", p.display());
                invalid.push(p.clone());
            }
        }
    }
    if metas.is_empty() {
        bail!("no importable export files among the given path(s)");
    }

    let db = open_db(override_path)?;

    // Flag sessions that already exist so the user knows what a re-import
    // means (upstream import refreshes rather than duplicates).
    let mut entries: Vec<(oc::transfer::ImportMeta, bool)> = Vec::new();
    for m in metas {
        let exists = oc::session::exists(&db, &m.session_id)
            .with_context(|| format!("checking for session {}", m.session_id))?;
        entries.push((m, exists));
    }

    // --- present the plan ---------------------------------------------------
    let plan: Vec<serde_json::Value> = entries
        .iter()
        .map(|(m, exists)| {
            serde_json::json!({
                "file": m.path,
                "session_id": m.session_id,
                "title": m.title,
                "already_in_db": exists,
            })
        })
        .collect();
    match output {
        Output::Human => {
            println!("import {} session(s):", entries.len());
            for (m, exists) in &entries {
                println!(
                    "  {id}  {title}{note} <- {file}",
                    id = m.session_id,
                    title = m.title.as_deref().unwrap_or(""),
                    note = if *exists { "  [already in db]" } else { "" },
                    file = m.path.display()
                );
            }
            if !invalid.is_empty() {
                println!("  ({} invalid file(s) skipped)", invalid.len());
            }
        }
        other => other.emit(
            &serde_json::json!({ "plan": plan, "invalid": invalid }),
            || {},
        ),
    }

    // --- confirm ------------------------------------------------------------
    if !yes && !output.is_json() {
        print!("run these imports? [y/N] ");
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

    // --- backup (import mutates the DB via the child process) ---------------
    if !no_backup {
        db.checkpoint().context("WAL checkpoint before backup")?;
        let bak = PathBuf::from(format!("{}.bak", db.path().display()));
        db.backup(&bak)
            .with_context(|| format!("backing up to {}", bak.display()))?;
        if !output.is_json() {
            eprintln!("backup   : {}", bak.display());
        }
    }

    // --- run ----------------------------------------------------------------
    let mut imported: Vec<(String, PathBuf)> = Vec::new();
    let mut failed: Vec<(PathBuf, String)> = Vec::new();
    for (m, _) in &entries {
        match oc::transfer::import_file(bin, override_path, &m.path) {
            Ok(msg) => {
                let line = if msg.is_empty() {
                    m.session_id.clone()
                } else {
                    msg
                };
                if !quiet && !output.is_json() {
                    println!("ok       : {line} <- {}", m.path.display());
                }
                imported.push((m.session_id.clone(), m.path.clone()));
            }
            Err(e) => {
                eprintln!("failed   : {}: {e}", m.path.display());
                failed.push((m.path.clone(), e.to_string()));
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
        output.emit(
            &serde_json::json!({
                "imported": imported,
                "failed": failed,
                "invalid_skipped": invalid,
            }),
            || {},
        );
    } else if !failed.is_empty() {
        bail!("{} of {} import(s) failed", failed.len(), entries.len());
    } else if !quiet {
        println!(
            "done     : {} imported, {} failed, {} skipped",
            imported.len(),
            failed.len(),
            invalid.len()
        );
    }
    Ok(())
}

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

// ---------------------------------------------------------------------------
// project handlers
// ---------------------------------------------------------------------------

fn cmd_project_list(db: &oc::Db, output: Output) -> Result<()> {
    let projects = oc::project::list(db).context("listing projects")?;
    output.emit(&projects, || {
        if projects.is_empty() {
            println!("(no projects)");
            return;
        }
        for p in &projects {
            let name = p.name.as_deref().unwrap_or("");
            println!("{}  {}  {}", p.id, p.worktree.display(), name);
        }
    });
    Ok(())
}

fn cmd_project_show(db: &oc::Db, id: &str, output: Output) -> Result<()> {
    let project = oc::project::get(db, id)
        .with_context(|| format!("looking up project {id:?}"))?
        .with_context(|| format!("no project with id {id:?}"))?;
    output.emit(&project, || {
        println!("id       : {}", project.id);
        println!("worktree : {}", project.worktree.display());
        if let Some(name) = &project.name {
            println!("name     : {name}");
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// human renderers
// ---------------------------------------------------------------------------

fn print_session_human(s: &oc::Session) {
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

/// Render a Unix-millis timestamp as a stable, locale-independent string.
/// (Deliberately not full ISO-8601-with-timezone to avoid pulling in `chrono`
/// for the MVP; the raw epoch-ms is always visible in JSON mode anyway.)
fn fmt_unix_ms(ms: i64) -> String {
    let secs = ms / 1000;
    let days = secs / 86400;
    format!("{ms}ms ({secs}s / day {days} since epoch)")
}

/// Render a Unix-millis timestamp as `YYYY-MM-DD` (UTC) — used by the
/// interactive menu. Pure integer math (days-since-epoch → civil date,
/// Howard Hinnant's algorithm) so no `chrono` dependency is needed.
fn fmt_date(ms: i64) -> String {
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
// cli tests
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
}
