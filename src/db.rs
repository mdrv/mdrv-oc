// ===========================================================================
// db.rs — locate, open, back up, and checkpoint the OpenCode SQLite DB.
//
// This is the layer that owns the `rusqlite::Connection`. All SQL lives behind
// methods here (or in `session.rs` / `project.rs`, which take a `&Db`), so the
// raw `Connection` never escapes the library — callers can't accidentally run
// a half-baked statement.
//
// OpenCode runs the DB in WAL mode with `foreign_keys=ON` and a busy timeout.
// We set the same pragmas on open so our writes are consistent with the TUI's
// expectations and so concurrent reads (e.g. an open TUI) don't immediately
// trip a lock error.
// ===========================================================================

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::{Error, Result};

/// Default relative location of the DB inside the data dir.
const DB_REL: &str = "opencode/opencode.db";

/// An open handle to the OpenCode database.
///
/// Wraps a single `rusqlite::Connection`. Cheap to create relative to the
/// query cost; for a CLI we open once and drop at the end of `main`.
pub struct Db {
    conn: Connection,
    /// Where the DB file lives on disk (kept for backup paths / messages).
    path: PathBuf,
}

impl Db {
    /// Open a database at an explicit path, applying OpenCode-compatible
    /// pragmas. The file must already exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Err(Error::DbNotFound {
                detail: format!("{} does not exist", path.display()),
            });
        }
        let conn = Connection::open(&path)?;
        Self::apply_pragmas(&conn)?;
        Ok(Db { conn, path })
    }

    /// Locate and open the default database (XDG data dir or `$HOME`).
    ///
    /// Resolution order for the *directory* containing `opencode/opencode.db`:
    ///   1. `$XDG_DATA_HOME` (if set and absolute);
    ///   2. `$HOME/.local/share`;
    ///   3. (error if neither is usable).
    pub fn open_default() -> Result<Self> {
        Self::open(Self::default_path()?)
    }

    /// The on-disk path of the open DB. Used for backups and user messages.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Borrow the underlying connection for the operation modules.
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// A mutable borrow, used by write operations (`session_move`).
    pub(crate) fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Compute the default DB path without opening it.
    fn default_path() -> Result<PathBuf> {
        if let Some(override_path) = std::env::var_os("OPENCODE_DB") {
            // An explicit env override wins, verbatim.
            return Ok(PathBuf::from(override_path));
        }
        let data_dir = std::env::var_os("XDG_DATA_HOME")
            .filter(|s| {
                // XDG spec: if set, it must be absolute or it is ignored.
                Path::new(s).is_absolute()
            })
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|h| h.join(".local/share"))
            })
            .ok_or(Error::DbNotFound {
                detail: "neither $XDG_DATA_HOME nor $HOME is set".into(),
            })?;

        Ok(data_dir.join(DB_REL))
    }

    /// Apply the same connection pragmas OpenCode uses.
    fn apply_pragmas(conn: &Connection) -> Result<()> {
        // `foreign_keys=ON` is essential: the schema is full of FK constraints
        // and OpenCode relies on them for cascade deletes. WAL matches the TUI.
        // busy_timeout lets us tolerate a briefly-locked DB instead of failing
        // instantly (e.g. another reader is mid-checkpoint).
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(())
    }

    /// Force WAL contents to be merged into the main DB file. Cheap, and makes
    /// a file-copy backup consistent (otherwise the `-wal` file holds recent
    /// commits). Mirrors the `PRAGMA wal_checkpoint(FULL)` from claude.md.
    pub fn checkpoint(&self) -> Result<()> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(FULL)", [], |_| Ok(()))?;
        Ok(())
    }

    /// Copy the DB file to `<path>.bak`, overwriting any prior backup. Should
    /// be preceded by [`Self::checkpoint`] so the copy isn't missing WAL data.
    pub fn backup(&self, to: &Path) -> Result<()> {
        std::fs::copy(&self.path, to)?;
        Ok(())
    }
}
