// ===========================================================================
// session.rs — read & mutate the `session` table.
//
// Reads (`list`, `resolve`) take `&Db`; the single write (`session_move`)
// takes `&mut Db` so the type system flags any accidental shared-borrow during
// a transaction.
// ===========================================================================

use std::path::Path;

use rusqlite::params;

use crate::db::Db;
use crate::model::Session;
use crate::{Error, Result};

use serde::Serialize;

/// What the user typed to identify a session. The same string is tried, in
/// order, as: exact id → id prefix → exact slug → title/slug substring. This
/// keeps the CLI ergonomic (`mdrv-oc session mv mighty-wizard ...`) while still
/// accepting full ids.
#[derive(Debug, Clone)]
pub struct SessionSelector {
    pub query: String,
}

impl SessionSelector {
    pub fn parse(s: impl Into<String>) -> Self {
        SessionSelector { query: s.into() }
    }
}

/// Fetch sessions, newest first. `limit` caps the result (the DB can hold
/// thousands). Pass `usize::MAX` (or a large number) for "all".
pub fn list(db: &Db, limit: u32) -> Result<Vec<Session>> {
    let mut stmt = db.conn().prepare(
        "SELECT id, project_id, parent_id, slug, directory, title,
                time_created, time_updated
         FROM session
         ORDER BY time_updated DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], row_to_session)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Case-insensitive substring search over `slug` and `title`, newest first —
/// backs `session export --filter`. SQLite's `LIKE` is already ASCII
/// case-insensitive, so no `lower()` juggling is needed.
pub fn search(db: &Db, needle: &str, limit: u32) -> Result<Vec<Session>> {
    let pattern = format!("%{}%", escape_like(needle));
    let mut stmt = db.conn().prepare(
        "SELECT id, project_id, parent_id, slug, directory, title,
                time_created, time_updated
         FROM session
         WHERE slug LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\'
         ORDER BY time_updated DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![pattern, limit as i64], row_to_session)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Does a session with this exact id exist? Used by `session import` to flag
/// re-imports before running them.
pub fn exists(db: &Db, id: &str) -> Result<bool> {
    let n: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM session WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Resolve a selector to exactly one session, or fail with `NotFound` /
/// `Ambiguous`. Tries the most specific matcher first and stops at the first
/// tier that yields results, so `ses_0b8dc33a7` (prefix) never falls through to
/// a broader substring search.
pub fn resolve(db: &Db, sel: &SessionSelector) -> Result<Session> {
    let q = &sel.query;

    // Tier 1 — exact id.
    if let Some(s) = find_where(db, "id = ?1", params![q])? {
        return Ok(s);
    }
    // Tier 2 — id prefix. `ses_…` ids share a prefix; LIKE it with an anchor.
    if let Some(mut hits) = find_many_where(
        db,
        "id LIKE ?1 ESCAPE '\\'",
        params![format!("{}%", escape_like(q))],
    )? {
        return exactly_one(&mut hits, q);
    }
    // Tier 3 — exact slug (slugs are unique-ish, e.g. "mighty-wizard").
    if let Some(s) = find_where(db, "slug = ?1", params![q])? {
        return Ok(s);
    }
    // Tier 4 — case-insensitive substring on slug OR title.
    if let Some(mut hits) = find_many_where(
        db,
        "slug LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\'",
        params![format!("%{}%", escape_like(q))],
    )? {
        return exactly_one(&mut hits, q);
    }

    Err(Error::NotFound {
        what: "session",
        query: q.clone(),
    })
}

/// The row changed by a [`session_move`] — returned so the CLI can report.
#[derive(Debug, Clone, Serialize)]
pub struct MoveResult {
    pub id: String,
    pub moved: bool,
}

/// Rewrite a session's `directory` (and optionally `project_id`), inside a
/// single transaction so the change is atomic.
///
/// - `new_dir` is the already-normalized absolute path.
/// - `new_project_id`, if `Some`, also sets `session.project_id`. The caller is
///   responsible for validating it exists (see `project::find_by_worktree`).
/// - `children`, if true, applies the same `directory`/`project_id` change to
///   every direct child (`parent_id = session.id`).
///
/// Returns `moved: false` (and commits nothing) if the row already has these
/// exact values — handy for idempotent dry-run-vs-apply flows.
pub fn session_move(
    db: &mut Db,
    id: &str,
    new_dir: &Path,
    new_project_id: Option<&str>,
    children: bool,
) -> Result<MoveResult> {
    let tx = db.conn_mut().transaction()?;

    // Read the current values so we can (a) report a no-op and (b) guard
    // against a non-existent session id producing a silent zero-row update.
    let (cur_dir, cur_pid): (String, Option<String>) = tx
        .query_row(
            "SELECT directory, project_id FROM session WHERE id = ?1",
            params![id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Error::NotFound {
                what: "session",
                query: id.to_string(),
            },
            other => Error::Sqlite(other),
        })?;

    let dir_str = new_dir.to_string_lossy();
    let dir_same = cur_dir == dir_str;
    let pid_same = match new_project_id {
        Some(np) => cur_pid.as_deref() == Some(np),
        None => true,
    };
    if dir_same && pid_same {
        tx.commit()?;
        return Ok(MoveResult {
            id: id.to_string(),
            moved: false,
        });
    }

    if let Some(npid) = new_project_id {
        tx.execute(
            "UPDATE session SET directory = ?1, project_id = ?2, time_updated = time_updated
             WHERE id = ?3",
            params![dir_str, npid, id],
        )?;
    } else {
        tx.execute(
            "UPDATE session SET directory = ?1, time_updated = time_updated WHERE id = ?2",
            params![dir_str, id],
        )?;
    }

    if children {
        if let Some(npid) = new_project_id {
            tx.execute(
                "UPDATE session SET directory = ?1, project_id = ?2 WHERE parent_id = ?3",
                params![dir_str, npid, id],
            )?;
        } else {
            tx.execute(
                "UPDATE session SET directory = ?1 WHERE parent_id = ?2",
                params![dir_str, id],
            )?;
        }
    }

    tx.commit()?;
    Ok(MoveResult {
        id: id.to_string(),
        moved: true,
    })
}

// --- helpers ---------------------------------------------------------------

/// Count direct children of a session — used by the CLI to nudge the user
/// about `--children`.
pub fn count_children(db: &Db, id: &str) -> Result<i64> {
    let n: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM session WHERE parent_id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    Ok(n)
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        parent_id: row.get("parent_id")?,
        slug: row.get("slug")?,
        directory: std::path::PathBuf::from(row.get::<_, String>("directory")?),
        title: row.get("title")?,
        time_created: row.get("time_created")?,
        time_updated: row.get("time_updated")?,
    })
}

/// One row or none, given a literal WHERE fragment + params.
fn find_where<P: rusqlite::Params>(db: &Db, where_sql: &str, params: P) -> Result<Option<Session>> {
    let sql = format!(
        "SELECT id, project_id, parent_id, slug, directory, title,
                time_created, time_updated
         FROM session WHERE {where_sql} LIMIT 1"
    );
    let mut stmt = db.conn().prepare(&sql)?;
    let mut rows = stmt.query(params)?;
    match rows.next()? {
        Some(r) => Ok(Some(row_to_session(r)?)),
        None => Ok(None),
    }
}

/// All rows matching a WHERE fragment, or `None` if there were zero.
fn find_many_where<P: rusqlite::Params>(
    db: &Db,
    where_sql: &str,
    params: P,
) -> Result<Option<Vec<Session>>> {
    let sql = format!(
        "SELECT id, project_id, parent_id, slug, directory, title,
                time_created, time_updated
         FROM session WHERE {where_sql} ORDER BY time_updated DESC LIMIT 50"
    );
    let mut stmt = db.conn().prepare(&sql)?;
    let rows = stmt.query_map(params, row_to_session)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

/// Collapse a tier's matches into one result: exactly one → Ok, more → the
/// `Ambiguous` error listing short labels (`id  slug  title`).
fn exactly_one(hits: &mut Vec<Session>, q: &str) -> Result<Session> {
    if hits.len() == 1 {
        Ok(hits.pop().unwrap())
    } else {
        let labels = hits
            .iter()
            .map(|s| format!("{}  ({})  {}", s.id, s.slug, s.title))
            .collect();
        Err(Error::Ambiguous {
            what: "session",
            query: q.to_string(),
            matches: labels,
        })
    }
}

/// Escape `%`, `_`, and `\` for use in a `LIKE … ESCAPE '\'` pattern.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}
