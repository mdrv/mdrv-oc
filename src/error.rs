// ===========================================================================
// error.rs — the library's typed error.
//
// Two error layers in this project (same split as `mdrv-ink`):
//   1. THIS file: a narrow, typed `Error` enum used by *library* internals.
//      Lets downstream callers `match` on a known, finite set of failures.
//   2. `anyhow::Error` (in `main.rs`/`cli.rs`): flexible, context-bearing error
//      for the *binary*. Auto-converts our `Error` at the `?` boundary because
//      we implement `std::error::Error` here.
//
// If you come from TypeScript: this is roughly a discriminated union
// `type Error = { kind: 'sqlite'; ... } | { kind: 'io'; ... }`, with the
// compiler enforcing every `match` is exhaustive.
// ===========================================================================

use std::fmt;

/// Every way the *library* can fail. Kept small — add a variant only when a
/// caller might reasonably want to react differently.
#[derive(Debug)]
pub enum Error {
    /// A filesystem failure (file missing, permission denied, ...). Wraps the
    /// underlying `std::io::Error` so the OS error code is preserved.
    Io(std::io::Error),

    /// A SQLite failure from `rusqlite`. Wraps `rusqlite::Error` verbatim.
    Sqlite(rusqlite::Error),

    /// A selector matched no rows. `what` names the table ("session"),
    /// `query` is what the user typed.
    NotFound { what: &'static str, query: String },

    /// A selector matched more than one row. `matches` is a short human label
    /// per candidate so the caller can list them.
    Ambiguous {
        what: &'static str,
        query: String,
        matches: Vec<String>,
    },

    /// The caller supplied something unusable (empty directory, a project id
    /// that doesn't exist, ...).
    InvalidInput { msg: String },

    /// The OpenCode data directory could not be located (no `HOME`/`USERPROFILE`,
    /// no `XDG_DATA_HOME`, and no `--db` override).
    DbNotFound { detail: String },

    /// A wrapped `opencode` child process (`export`/`import`) exited non-zero.
    /// `detail` carries the child's captured stderr/stdout for display.
    External { cmd: String, detail: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Sqlite(e) => write!(f, "SQLite error: {e}"),
            Error::NotFound { what, query } => {
                write!(f, "no {what} matched {query:?}")
            }
            Error::Ambiguous {
                what,
                query,
                matches,
            } => {
                write!(f, "{query:?} matched {} {what}(s):", matches.len())?;
                for m in matches {
                    write!(f, "\n  • {m}")?;
                }
                Ok(())
            }
            Error::InvalidInput { msg } => write!(f, "{msg}"),
            Error::DbNotFound { detail } => {
                write!(f, "could not locate opencode.db: {detail}")
            }
            Error::External { cmd, detail } => {
                write!(f, "command failed: {cmd}")?;
                if !detail.is_empty() {
                    write!(f, "\n{detail}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Sqlite(e) => Some(e),
            _ => None,
        }
    }
}

/// `From<std::io::Error>` makes `fs::...(path)?` compile inside `Result<T>`.
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// `From<rusqlite::Error>` makes every `conn.query_row(...)?` Just Work.
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Sqlite(e)
    }
}

/// Project-wide `Result` alias. Returning `Result<T>` (instead of the verbose
/// `Result<T, Error>`) is the common Rust idiom — mirrored from `mdrv-ink`.
pub type Result<T> = std::result::Result<T, Error>;
