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
                  MVP: move a session to another directory. See README.md and \
                  claude.md for background."
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
