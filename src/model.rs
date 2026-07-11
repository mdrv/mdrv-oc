// ===========================================================================
// model.rs — data types the library reads from / writes to the DB.
//
// Plain structs mirroring the OpenCode schema. We model only the columns we
// actually use (a full ORM of every table is out of scope for the MVP); adding
// a field is a one-line change here + the matching `row.get` in `db.rs`.
//
// Everything derives `serde::Serialize` so the `--json` CLI mode emits these
// unchanged. Deserialize is intentionally NOT derived: these are DB rows, not
// request bodies — we never reconstruct them from JSON.
// ===========================================================================

use std::path::PathBuf;

use serde::Serialize;

/// One row of the `session` table — the subset of columns `mdrv-oc` cares
/// about. The session's working directory lives in `directory` (NOT NULL),
/// independent of its `project_id`.
#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    pub project_id: String,
    /// Parent session id, if this is a sub-session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub slug: String,
    /// The directory OpenCode launches from for this session. This is the
    /// field `session mv` rewrites.
    pub directory: PathBuf,
    pub title: String,
    /// Unix-millis timestamps.
    pub time_created: i64,
    pub time_updated: i64,
}

/// One row of the `project` table. `worktree` is the path OpenCode associates
/// with the project id (the initial launch directory, or a cached git worktree).
#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub id: String,
    pub worktree: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub time_created: i64,
    pub time_updated: i64,
}
