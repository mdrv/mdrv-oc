// ===========================================================================
// cli.rs — the `mdrv-oc` command-line interface.
//
// Part of the *binary*, not the library: it lives in `src/` but is only pulled
// in by `src/main.rs` (`mod cli;`). That keeps `clap` out of the reusable core
// so the library stays embeddable.
//
// This file owns the clap command tree and the dispatch. The handlers live in
// `src/commands/` (one module per command family); `src/commands/util.rs`
// carries the shared output-mode / prompt / formatting plumbing.
// ===========================================================================

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use mdrv_oc as oc;

use crate::commands::{
    cmd_project_list, cmd_project_show, cmd_session_export, cmd_session_import,
    cmd_session_inspect, cmd_session_list, cmd_session_move, cmd_session_show, cmd_session_sync,
    open_db, Output,
};

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

    /// Show one session (exact id, id prefix, slug, or title substring),
    /// including model, agent, message count, cost and token usage.
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
        /// Falls back to $MDRV_OC_OUT, then to the current directory.
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,

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
    /// id, so `opencode -s <id>` resumes it; by default it is re-anchored to
    /// the project of the current working directory — override with --into.
    Import {
        /// Export file(s) or directory(ies) containing export files.
        files: Vec<PathBuf>,

        /// Re-anchor imported sessions to this directory's project instead
        /// of the current working directory.
        #[arg(long, value_name = "DIR")]
        into: Option<PathBuf>,

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

    /// Two-way convergence with a synced directory (pull, then push).
    ///
    /// DIR is your Unison/syncthing-shared export folder. `sync` indexes it
    /// the way `import` does, imports files that are newer than their DB
    /// copy (pull), then exports sessions that are newer than their on-disk
    /// copy (push). Everything already current is skipped, so running it on
    /// both devices after each sync converges them. A preview lists both
    /// directions before anything runs.
    Sync {
        /// The synced directory holding the export files.
        dir: PathBuf,

        /// Limit the push side to sessions whose slug/title contains TEXT.
        #[arg(long, value_name = "TEXT")]
        filter: Option<String>,

        /// Write plain `.json` for new exports instead of `.json.zst`.
        #[arg(long)]
        no_compress: bool,

        /// Skip the confirmation prompt (runs the recommended plan).
        #[arg(short = 'y', long)]
        yes: bool,

        /// Skip the automatic `<db>.bak` backup before the pull.
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
                into,
                yes,
                no_backup,
                opencode_bin,
            } => cmd_session_import(
                cli.db.as_deref(),
                files,
                into,
                yes,
                no_backup,
                &opencode_bin,
                cli.quiet,
                output,
            )?,
            SessionCommand::Sync {
                dir,
                filter,
                no_compress,
                yes,
                no_backup,
                opencode_bin,
            } => cmd_session_sync(
                cli.db.as_deref(),
                dir,
                filter,
                !no_compress,
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
