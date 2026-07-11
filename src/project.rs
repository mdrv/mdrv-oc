// ===========================================================================
// project.rs — read the `project` table and infer the right `project_id` for a
// given directory.
//
// The schema has two ways a directory can be associated with a project:
//   1. `project.worktree`            — the canonical path for that project id;
//   2. `project_directory(directory)` — a (project_id, directory) link table,
//                                       one project can own several dirs.
// We check both, preferring an exact `worktree` match.
// ===========================================================================

use std::path::Path;

use rusqlite::params;

use crate::db::Db;
use crate::model::Project;
use crate::Result;

/// Every project row, ordered by most-recently-touched. The table is small, so
/// we don't bother paginating.
pub fn list(db: &Db) -> Result<Vec<Project>> {
    let mut stmt = db.conn().prepare(
        "SELECT id, worktree, name, time_created, time_updated
         FROM project ORDER BY time_updated DESC",
    )?;
    let rows = stmt.query_map([], row_to_project)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Find the best project id for a directory. Prefers an exact `worktree` match;
/// falls back to the `project_directory` link table. Returns `None` if the
/// directory is unknown to OpenCode (the "launch once, quit" trick from
/// claude.md is the way to make it known).
pub fn find_by_directory(db: &Db, dir: &Path) -> Result<Option<Project>> {
    let dir_str = dir.to_string_lossy();

    // 1. exact worktree match.
    if let Some(p) = one(
        db,
        "SELECT id, worktree, name, time_created, time_updated
         FROM project WHERE worktree = ?1",
        params![dir_str.as_ref()],
    )? {
        return Ok(Some(p));
    }

    // 2. project_directory link table. A directory can be linked to at most one
    //    project in practice; we take the most recent.
    if let Some(p) = one(
        db,
        "SELECT p.id, p.worktree, p.name, p.time_created, p.time_updated
         FROM project_directory pd
         JOIN project p ON p.id = pd.project_id
         WHERE pd.directory = ?1
         ORDER BY pd.time_created DESC LIMIT 1",
        params![dir_str.as_ref()],
    )? {
        return Ok(Some(p));
    }

    Ok(None)
}

/// Look up a single project by exact id (used to validate `--project <ID>`).
pub fn get(db: &Db, id: &str) -> Result<Option<Project>> {
    one(
        db,
        "SELECT id, worktree, name, time_created, time_updated
         FROM project WHERE id = ?1",
        params![id],
    )
}

fn row_to_project(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get("id")?,
        worktree: std::path::PathBuf::from(row.get::<_, String>("worktree")?),
        name: row.get("name")?,
        time_created: row.get("time_created")?,
        time_updated: row.get("time_updated")?,
    })
}

fn one<P: rusqlite::Params>(db: &Db, sql: &str, params: P) -> Result<Option<Project>> {
    let mut stmt = db.conn().prepare(sql)?;
    let mut rows = stmt.query(params)?;
    match rows.next()? {
        Some(r) => Ok(Some(row_to_project(r)?)),
        None => Ok(None),
    }
}
