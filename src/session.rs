// ===========================================================================
// session.rs — read & mutate the sessions table.
//
// OpenCode v2 keeps sessions in `session_v2` (a migrated superset of the
// legacy `session` table) and stops writing the legacy one; which table is
// active is resolved per query via `Db::session_table`. Reads (`list`,
// `resolve`) take `&Db`; the single write (`session_move`) takes `&mut Db` so
// the type system flags any accidental shared-borrow during a transaction.
// ===========================================================================

use std::path::Path;

use rusqlite::params;

use crate::db::Db;
use crate::model::Session;
use crate::{Error, Result};

use serde::Serialize;

/// The session columns mdrv-oc reads — identical names in `session` (v1)
/// and `session_v2` (v2).
const SESSION_COLS: &str =
    "id, project_id, parent_id, slug, directory, title, time_created, time_updated";

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
    let table = crate::db::session_table(db.conn())?;
    let sql = format!("SELECT {SESSION_COLS} FROM {table} ORDER BY time_updated DESC LIMIT ?1");
    let mut stmt = db.conn().prepare(&sql)?;
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
    let table = crate::db::session_table(db.conn())?;
    let pattern = format!("%{}%", escape_like(needle));
    let sql = format!(
        "SELECT {SESSION_COLS} FROM {table}
         WHERE slug LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\'
         ORDER BY time_updated DESC
         LIMIT ?2"
    );
    let mut stmt = db.conn().prepare(&sql)?;
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
    let table = crate::db::session_table(db.conn())?;
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE id = ?1");
    let n: i64 = db.conn().query_row(&sql, params![id], |r| r.get(0))?;
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
    // Tier 3 — exact slug. Slugs can collide (v2 migration / imports keep
    // them), so route through exactly_one instead of grabbing an arbitrary
    // row — ties demand disambiguation with a full id.
    if let Some(mut hits) = find_many_where(db, "slug = ?1", params![q])? {
        return exactly_one(&mut hits, q);
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
/// - `new_project_id`, if `Some`, also sets `project_id`. The caller is
///   responsible for validating it exists (see `project::find_by_worktree`).
/// - `children`, if true, applies the same `directory`/`project_id` change to
///   every direct child (`parent_id = session.id`).
/// - On an OpenCode v2 database the write is mirrored to the legacy `session`
///   table whenever it also holds the row (v2 migrated everything there once
///   and then froze it), so v1-era tooling stays consistent.
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
    let conn = db.conn_mut();
    // The active table: `session_v2` on v2 databases, legacy `session` on v1.
    let active = crate::db::session_table(conn)?;
    let tx = conn.transaction()?;

    // Read the current values so we can (a) report a no-op and (b) guard
    // against a non-existent session id producing a silent zero-row update.
    let select = format!("SELECT directory, project_id FROM {active} WHERE id = ?1");
    let (cur_dir, cur_pid): (String, Option<String>) = tx
        .query_row(&select, params![id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
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

    let mut tables: Vec<&str> = vec![active];
    if active == "session_v2" {
        tables.push("session");
    }

    for t in &tables {
        if let Some(npid) = new_project_id {
            tx.execute(
                &format!("UPDATE {t} SET directory = ?1, project_id = ?2 WHERE id = ?3"),
                params![dir_str, npid, id],
            )?;
        } else {
            tx.execute(
                &format!("UPDATE {t} SET directory = ?1 WHERE id = ?2"),
                params![dir_str, id],
            )?;
        }
    }

    if children {
        for t in &tables {
            if let Some(npid) = new_project_id {
                tx.execute(
                    &format!("UPDATE {t} SET directory = ?1, project_id = ?2 WHERE parent_id = ?3"),
                    params![dir_str, npid, id],
                )?;
            } else {
                tx.execute(
                    &format!("UPDATE {t} SET directory = ?1 WHERE parent_id = ?2"),
                    params![dir_str, id],
                )?;
            }
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
    let table = crate::db::session_table(db.conn())?;
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE parent_id = ?1");
    let n: i64 = db.conn().query_row(&sql, params![id], |r| r.get(0))?;
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
    let table = crate::db::session_table(db.conn())?;
    let sql = format!("SELECT {SESSION_COLS} FROM {table} WHERE {where_sql} LIMIT 1");
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
    let table = crate::db::session_table(db.conn())?;
    let sql = format!(
        "SELECT {SESSION_COLS} FROM {table}
         WHERE {where_sql} ORDER BY time_updated DESC LIMIT 50"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    /// Legacy (v1-era) schema — exactly the columns mdrv-oc reads.
    const LEGACY_DDL: &str = "CREATE TABLE session (
        id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, slug TEXT,
        directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER)";

    /// Minimal slice of the v2 table — the columns mdrv-oc reads, plus the
    /// v2-only ones that sit between them in the real schema.
    const V2_DDL: &str = "CREATE TABLE session_v2 (
        id TEXT PRIMARY KEY, project_id TEXT, workspace_id TEXT, parent_id TEXT,
        fork_session_id TEXT, slug TEXT, directory TEXT, title TEXT,
        time_created INTEGER, time_updated INTEGER)";

    fn temp_db(name: &str) -> Db {
        let dir = std::env::temp_dir().join("mdrv-oc-test-session");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        // Db::open requires the file to exist (it never creates a DB).
        std::fs::File::create(&path).unwrap();
        Db::open(&path).unwrap()
    }

    fn insert(
        conn: &rusqlite::Connection,
        table: &str,
        id: &str,
        slug: &str,
        dir: &str,
        updated: i64,
    ) {
        conn.execute(
            &format!(
                "INSERT INTO {table} (id, project_id, slug, directory, title,
                                      time_created, time_updated)
                 VALUES (?1, 'p', ?2, ?3, 't', 0, ?4)"
            ),
            params![id, slug, dir, updated],
        )
        .unwrap();
    }

    fn directory_of(db: &Db, table: &str, id: &str) -> String {
        db.conn()
            .query_row(
                &format!("SELECT directory FROM {table} WHERE id = ?1"),
                params![id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// v1-era database: no `session_v2` table → legacy `session` is read
    /// and written.
    #[test]
    fn legacy_db_reads_and_moves_session_table() {
        let mut db = temp_db("legacy.db");
        db.conn().execute_batch(LEGACY_DDL).unwrap();
        insert(db.conn(), "session", "ses_old", "old-craft", "/x/a", 100);

        assert!(exists(&db, "ses_old").unwrap());
        assert_eq!(list(&db, 10).unwrap()[0].slug, "old-craft");
        assert!(resolve(&db, &SessionSelector::parse("old-craft")).is_ok());

        let r = session_move(&mut db, "ses_old", Path::new("/x/b"), None, false).unwrap();
        assert!(r.moved);
        assert_eq!(directory_of(&db, "session", "ses_old"), "/x/b");
    }

    /// v2 database: `session_v2` is read (un-hiding v2-era sessions that
    /// legacy-only readers can't see) and moves are mirrored into the legacy
    /// table when it holds the row.
    #[test]
    fn v2_db_reads_session_v2_and_mirrors_moves() {
        let mut db = temp_db("v2.db");
        db.conn()
            .execute_batch(&format!("{LEGACY_DDL}; {V2_DDL}"))
            .unwrap();
        // migrated session: present in both tables (v2 froze the legacy copy)
        insert(db.conn(), "session", "ses_mig", "migrated-one", "/x/a", 50);
        insert(
            db.conn(),
            "session_v2",
            "ses_mig",
            "migrated-one",
            "/x/a",
            50,
        );
        // v2-era session: exists ONLY in session_v2
        insert(
            db.conn(),
            "session_v2",
            "ses_new",
            "fresh-import",
            "/x/c",
            200,
        );

        assert!(exists(&db, "ses_new").unwrap());
        assert_eq!(
            resolve(&db, &SessionSelector::parse("fresh-import"))
                .unwrap()
                .id,
            "ses_new"
        );
        assert_eq!(list(&db, 10).unwrap().len(), 2, "both rows visible");

        // moving the migrated session updates BOTH tables
        session_move(&mut db, "ses_mig", Path::new("/x/b"), None, false).unwrap();
        assert_eq!(directory_of(&db, "session", "ses_mig"), "/x/b");
        assert_eq!(directory_of(&db, "session_v2", "ses_mig"), "/x/b");

        // moving the v2-only session: legacy update is a harmless no-op
        session_move(&mut db, "ses_new", Path::new("/x/d"), None, false).unwrap();
        assert_eq!(directory_of(&db, "session_v2", "ses_new"), "/x/d");
        let n: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM session WHERE id = 'ses_new'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "no legacy row should be created");

        // slug collisions (real: v2 migration keeps old slugs around) must
        // resolve to Ambiguous, never to an arbitrary row
        insert(
            db.conn(),
            "session_v2",
            "ses_tie",
            "fresh-import",
            "/x/e",
            300,
        );
        let err = resolve(&db, &SessionSelector::parse("fresh-import")).unwrap_err();
        assert!(err.to_string().contains("matched 2 session(s)"), "{err}");
    }
}
