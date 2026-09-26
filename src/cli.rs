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

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
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

    /// Export session(s) to portable JSON files via the `opencode` CLI.
    ///
    /// Pick sessions with selectors (id/prefix/slug/title), `--filter` (title
    /// or slug substring), or — with neither — an interactive multi-select
    /// menu. Each session becomes one self-describing file in --out, named
    /// `YYYY-MM-DD_<slug>_ses_<id8>.json[.zst]` (local-time session start;
    /// plain `.json` with --no-compress, or a custom name via --name / the
    /// interactive prompt). Re-exports converge: the out dir is indexed by
    /// the session ids found inside the files, a newer session overwrites
    /// its older copy, and equal/newer copies are skipped.
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

        /// Custom file name (stem) for the export — single-session exports
        /// only. Sanitized; `.json[.zst]` is appended automatically.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,

        /// Write plain `.json` instead of zstd-compressed `.json.zst`.
        #[arg(long)]
        no_compress: bool,

        /// Path to the opencode binary to drive.
        #[arg(long, default_value = "opencode")]
        opencode_bin: PathBuf,

        /// Skip the bulk confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Import session export file(s) via the `opencode` CLI.
    ///
    /// Takes one or more export files or directories (a directory covers
    /// every `*.json` / `*.json.zst` inside it). `.json.zst` files are
    /// detected by magic bytes and decompressed transparently. The preview
    /// dedupes multiple files of the same session (newest wins), flags
    /// sessions already in the DB (stale re-imports are skipped by default),
    /// and then asks which rows to import. The imported session keeps its
    /// id, so `opencode -s <id>` resumes it; like upstream import, it is
    /// re-anchored to the project of the current working directory.
    Import {
        /// Export file(s) or directory(ies) containing export files.
        files: Vec<PathBuf>,

        /// Skip the confirmation prompt (imports the recommended set: new
        /// sessions plus files newer than the DB copy).
        #[arg(short = 'y', long)]
        yes: bool,

        /// Skip the automatic `<db>.bak` backup.
        #[arg(long)]
        no_backup: bool,

        /// Path to the opencode binary to drive.
        #[arg(long, default_value = "opencode")]
        opencode_bin: PathBuf,
    },

    /// Peek inside export file(s) or directory(ies) without importing.
    ///
    /// Prints what each `*.json[.zst]` export contains — session id, title,
    /// working directory, start date and size — read straight from the
    /// (transparently decompressed) file. `[in db]` flags sessions already
    /// present in the local database. Handles both the v1 and the v2 export
    /// layout, with no `opencode` subprocess and no DB writes.
    Inspect {
        /// Export file(s) or directory(ies) containing export files.
        paths: Vec<PathBuf>,
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
                name,
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
                name,
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
            SessionCommand::Inspect { paths } => {
                cmd_session_inspect(cli.db.as_deref(), paths, output)?
            }
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
/// then loop `opencode export <id>` writing one self-describing file per
/// session into --out: `YYYY-MM-DD_<slug>_ses_<id8>.json[.zst]` (local-time
/// session start, or a custom name via --name / the interactive prompt).
///
/// The out dir is indexed by the session ids found *inside* the existing
/// files, so re-exports converge (newer wins) and forgotten copies of the
/// same session — under any naming — are reported before writing.
#[allow(clippy::too_many_arguments)]
fn cmd_session_export(
    override_path: Option<&std::path::Path>,
    selectors: Vec<String>,
    filter: Option<String>,
    limit: u32,
    out: PathBuf,
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

    let out_dir = oc::pathutil::normalize_directory(&out.to_string_lossy())?;
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

        // Newer-wins against what is already on disk.
        let mut retire: Option<PathBuf> = None;
        let dest: PathBuf =
            match oc::transfer::decide_export(index.newest_for(&s.id), Some(s.time_updated)) {
                oc::transfer::ExportDecision::SkipUpToDate(p) => {
                    if !quiet && !output.is_json() {
                        println!(
                            "= up-to-date: {} — {} is already current",
                            s.id,
                            file_label(&p)
                        );
                    }
                    skipped.push(serde_json::json!({
                        "id": s.id, "file": p, "reason": "up-to-date"
                    }));
                    continue;
                }
                oc::transfer::ExportDecision::SkipStale(p) => {
                    if !quiet && !output.is_json() {
                        println!(
                            "< skipped   : {} — {} is newer than the session row",
                            s.id,
                            file_label(&p)
                        );
                    }
                    skipped.push(serde_json::json!({
                        "id": s.id, "file": p, "reason": "existing-copy-newer"
                    }));
                    continue;
                }
                oc::transfer::ExportDecision::Overwrite(old) => {
                    // Same session, same stem, current format; when the format
                    // flipped (--no-compress or back) the old artifact is
                    // retired once the new one is on disk.
                    let stem_old = export_stem_of(&old);
                    let dest = out_dir.join(oc::transfer::export_filename(&stem_old, compress));
                    let flipped = dest != old;
                    let old_label = file_label(&old);
                    if flipped {
                        retire = Some(old);
                    }
                    if !quiet && !output.is_json() {
                        println!(
                            "> overwrite : {} — {} is older{}",
                            s.id,
                            old_label,
                            if flipped { " (format changed)" } else { "" }
                        );
                    }
                    dest
                }
                oc::transfer::ExportDecision::Write => {
                    let (unique, n) = oc::transfer::unique_stem(&stem, compress, &taken);
                    if n > 1 && !quiet && !output.is_json() {
                        println!(
                            "! renamed   : {stem} is claimed by another export — using {unique}"
                        );
                    }
                    let file_name = oc::transfer::export_filename(&unique, compress);
                    taken.insert(file_name.clone());
                    out_dir.join(file_name)
                }
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

/// `session import` — peek export files, dedupe same-session files to the
/// newest, flag sessions already in the DB (stale re-imports are skipped by
/// default), let the user pick rows, back up, then loop
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

    // --- peek every candidate file -----------------------------------------
    let mut peeked: Vec<oc::transfer::ExportInfo> = Vec::new();
    let mut invalid: Vec<PathBuf> = Vec::new();
    for p in &paths {
        match oc::transfer::peek_export_file(p) {
            Ok(m) => peeked.push(m),
            Err(e) => {
                eprintln!("skipping {}: {e}", p.display());
                invalid.push(p.clone());
            }
        }
    }
    if peeked.is_empty() {
        bail!("no importable export files among the given path(s)");
    }

    // --- dedupe by session id — the newest file wins ------------------------
    // superseded[i]: None if peeked[i] is its session's kept copy, else the
    // index of the copy that supersedes it.
    let superseded = oc::transfer::dedupe_newest(&peeked);
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

    let db = open_db(override_path)?;

    let mut rows: Vec<ImportRow> = Vec::with_capacity(peeked.len());
    for (i, e) in peeked.iter().enumerate() {
        let status = match superseded[i] {
            None => {
                let exists = oc::session::exists(&db, &e.session_id)
                    .with_context(|| format!("checking for session {}", e.session_id))?;
                if !exists {
                    RowStatus::New
                } else {
                    let db_updated =
                        oc::session::resolve(&db, &oc::SessionSelector::parse(&e.session_id))
                            .with_context(|| format!("loading session {}", e.session_id))?
                            .time_updated;
                    match e.time_updated {
                        Some(f) if f > db_updated => RowStatus::Refresh,
                        Some(f) if f == db_updated => RowStatus::UpToDate,
                        Some(_) => RowStatus::Stale,
                        None => RowStatus::UpToDate, // file age unknown — don't clobber by default
                    }
                }
            }
            Some(k) => RowStatus::Duplicate(row_of[&k]),
        };
        rows.push(ImportRow {
            info: e.clone(),
            status,
        });
    }

    // --- present the plan ---------------------------------------------------
    let plan: Vec<serde_json::Value> = rows
        .iter()
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
        .collect();
    match output {
        Output::Human => {
            println!("import {} session(s):", rows.len());
            for (r, row) in rows.iter().enumerate() {
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
            if !invalid.is_empty() {
                println!("  ({} invalid file(s) skipped)", invalid.len());
            }
        }
        other => other.emit(
            &serde_json::json!({ "plan": plan, "invalid": invalid }),
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
        db.checkpoint().context("WAL checkpoint before backup")?;
        let bak = PathBuf::from(format!("{}.bak", db.path().display()));
        db.backup(&bak)
            .with_context(|| format!("backing up to {}", bak.display()))?;
        if !output.is_json() {
            eprintln!("backup   : {}", bak.display());
        }
    }

    // --- run ----------------------------------------------------------------
    let flavor = oc::transfer::detect_flavor(bin);
    let mut imported: Vec<(String, PathBuf)> = Vec::new();
    let mut failed: Vec<(PathBuf, String)> = Vec::new();
    for r in &chosen {
        let row = &rows[r - 1];
        match oc::transfer::import_file(bin, override_path, &row.info.path, flavor) {
            Ok(msg) => {
                let line = if msg.is_empty() {
                    row.info.session_id.clone()
                } else {
                    msg
                };
                if !quiet && !output.is_json() {
                    println!("ok       : {line} <- {}", row.info.path.display());
                }
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

/// `session inspect` — peek inside export file(s)/dir(s) without importing:
/// session id, title, working directory, start date, size, and whether the
/// session already exists in the local DB. No DB writes, no `opencode`
/// subprocess; handles both the v1 and the v2 export layout.
fn cmd_session_inspect(
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

    let mut infos: Vec<oc::transfer::ExportInfo> = Vec::new();
    let mut invalid: Vec<PathBuf> = Vec::new();
    for p in &expanded {
        match oc::transfer::peek_export_file(p) {
            Ok(i) => infos.push(i),
            Err(e) => {
                eprintln!("skipping {}: {e}", p.display());
                invalid.push(p.clone());
            }
        }
    }
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
// small interactive / formatting helpers
// ---------------------------------------------------------------------------

/// Print `prompt`, read one line, interpret it as the codebase's standard
/// `[y/N]` answer. EOF (closed stdin) counts as "no".
fn ask_yes(prompt: &str) -> Result<bool> {
    Ok(match prompt_line(prompt)? {
        Some(line) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        None => false,
    })
}

/// Print `prompt` and read one line from stdin; `None` on EOF.
fn prompt_line(prompt: &str) -> Result<Option<String>> {
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

/// `12 B` / `34.1 KB` / `5.6 MB` — compact size for listings.
fn fmt_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b < 1024.0 {
        format!("{bytes} B")
    } else if b < 1024.0 * 1024.0 {
        format!("{:.1} KB", b / 1024.0)
    } else {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    }
}

/// Last path segment, for compact listings.
fn file_label(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Stem of an export file name: `x.json.zst` / `x.json` → `x`.
fn export_stem_of(p: &std::path::Path) -> String {
    let name = file_label(p);
    name.strip_suffix(".json.zst")
        .or_else(|| name.strip_suffix(".json"))
        .unwrap_or(&name)
        .to_string()
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
