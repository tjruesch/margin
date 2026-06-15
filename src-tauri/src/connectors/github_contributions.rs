//! Provider-agnostic storage layer for GitHub contributions (#165).
//!
//! The `github` connector maps Search-API JSON into `Contribution` and
//! calls `upsert_contributions` to persist into `github_contributions`.
//! The changelog UI (`list_contributions`) and the AI `search_changelog`
//! tool (`search_contributions`) read back through this module without
//! caring how the data arrived — the `connector_id` foreign key carries
//! that.
//!
//! Accumulate-only: re-syncing a rolling window never deletes rows that
//! aged out, so the changelog is a growing historical record. The only
//! mutation on re-sync is an in-place UPDATE (a PR flipping open→merged).

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

/// One contribution row. `kind` is `"pr"` or `"commit"`; `state` is
/// `"merged"` / `"open"` / `"closed"` (PRs) or `"committed"` (commits).
#[derive(Debug, Clone, Serialize)]
pub struct Contribution {
    pub id: String,
    pub connector_id: String,
    pub external_id: String,
    pub kind: String,
    pub state: String,
    pub title: String,
    pub body: Option<String>,
    pub repo: String,
    pub url: String,
    pub author_login: String,
    pub author_avatar_url: Option<String>,
    pub created_at_ms: i64,
    pub merged_at_ms: Option<i64>,
    pub modified_ms: i64,
    /// AI changelog insight (#165 follow-up), generated lazily on first
    /// detail-view open. `ai_summary` is a plain "what was implemented";
    /// `ai_highlight` is JSON {"angle","content"} when a high-bar
    /// blog/LinkedIn angle exists, else NULL. `ai_generated_ms` NULL
    /// means not generated yet.
    pub ai_summary: Option<String>,
    pub ai_highlight: Option<String>,
    pub ai_generated_ms: Option<i64>,
}

#[derive(Debug, Default, Clone)]
pub struct UpsertReport {
    pub added: u64,
    pub updated: u64,
}

/// Upsert a batch of contributions for `connector_id`. Accumulate-only
/// (no orphan deletion). Emits a `github_contribution` event the first
/// time each contribution is seen so the activity stream / profile
/// worker pick it up. Runs in a single transaction.
pub fn upsert_contributions(
    conn: &mut Connection,
    connector_id: &str,
    items: &[Contribution],
) -> rusqlite::Result<UpsertReport> {
    let tx = conn.transaction()?;

    // Self team member id for event attribution — these are the user's
    // own contributions, so actor is always self. NULL when there's no
    // `is_self` row yet.
    let self_id: Option<String> = tx
        .query_row(
            "SELECT id FROM team_members WHERE is_self = 1 LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();

    let mut report = UpsertReport::default();
    for c in items {
        let pre_existed: bool = tx
            .query_row(
                "SELECT 1 FROM github_contributions WHERE id = ?1",
                params![c.id],
                |r| r.get::<_, i64>(0),
            )
            .ok()
            .is_some();

        tx.execute(
            "INSERT INTO github_contributions(\
                id, connector_id, external_id, kind, state, title, body, repo, url, \
                author_login, author_avatar_url, created_at_ms, merged_at_ms, modified_ms\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
             ON CONFLICT(id) DO UPDATE SET \
                state = excluded.state, \
                title = excluded.title, \
                body = excluded.body, \
                repo = excluded.repo, \
                url = excluded.url, \
                author_login = excluded.author_login, \
                author_avatar_url = excluded.author_avatar_url, \
                created_at_ms = excluded.created_at_ms, \
                merged_at_ms = excluded.merged_at_ms, \
                modified_ms = excluded.modified_ms",
            params![
                c.id,
                c.connector_id,
                c.external_id,
                c.kind,
                c.state,
                c.title,
                c.body,
                c.repo,
                c.url,
                c.author_login,
                c.author_avatar_url,
                c.created_at_ms,
                c.merged_at_ms,
                c.modified_ms,
            ],
        )?;

        if pre_existed {
            report.updated += 1;
        } else {
            let ts = c.merged_at_ms.unwrap_or(c.created_at_ms);
            let payload = serde_json::json!({
                "title": c.title,
                "repo": c.repo,
                "kind": c.kind,
                "state": c.state,
                "url": c.url,
            });
            crate::events::emit(
                &tx,
                ts,
                "github_contribution",
                self_id.as_deref(),
                "github",
                &c.id,
                &payload,
            )?;
            report.added += 1;
        }
    }

    tx.commit()?;
    Ok(report)
}

fn row_to_contribution(r: &rusqlite::Row<'_>) -> rusqlite::Result<Contribution> {
    Ok(Contribution {
        id: r.get(0)?,
        connector_id: r.get(1)?,
        external_id: r.get(2)?,
        kind: r.get(3)?,
        state: r.get(4)?,
        title: r.get(5)?,
        body: r.get(6)?,
        repo: r.get(7)?,
        url: r.get(8)?,
        author_login: r.get(9)?,
        author_avatar_url: r.get(10)?,
        created_at_ms: r.get(11)?,
        merged_at_ms: r.get(12)?,
        modified_ms: r.get(13)?,
        ai_summary: r.get(14)?,
        ai_highlight: r.get(15)?,
        ai_generated_ms: r.get(16)?,
    })
}

const SELECT_COLS: &str = "id, connector_id, external_id, kind, state, title, body, repo, url, \
     author_login, author_avatar_url, created_at_ms, merged_at_ms, modified_ms, \
     ai_summary, ai_highlight, ai_generated_ms";

/// List pull requests for the changelog feed, newest-first by their
/// effective timestamp (merge time for merged PRs, else creation time).
/// Commits are excluded — the changelog is PR-only (#165 follow-up).
pub fn list_contributions(
    conn: &Connection,
    limit: usize,
) -> rusqlite::Result<Vec<Contribution>> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM github_contributions WHERE kind = 'pr' \
         ORDER BY COALESCE(merged_at_ms, created_at_ms) DESC LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![limit as i64], row_to_contribution)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// One contribution by id (for the detail view / insight command).
pub fn get_contribution(conn: &Connection, id: &str) -> rusqlite::Result<Option<Contribution>> {
    let sql = format!("SELECT {SELECT_COLS} FROM github_contributions WHERE id = ?1");
    conn.query_row(&sql, params![id], row_to_contribution)
        .optional()
}

/// Persist a generated AI insight onto a contribution. `highlight_json`
/// is the serialized {"angle","content"} or None when nothing cleared
/// the bar.
pub fn set_ai_insight(
    conn: &Connection,
    id: &str,
    summary: &str,
    highlight_json: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE github_contributions \
         SET ai_summary = ?2, ai_highlight = ?3, ai_generated_ms = ?4 \
         WHERE id = ?1",
        params![id, summary, highlight_json, now_ms],
    )?;
    Ok(())
}

/// Delete this connector's non-PR rows. Called each sync so the
/// historical commit rows from the pre-PR-only build drain away.
/// Returns the number removed.
pub fn prune_non_prs(conn: &Connection, connector_id: &str) -> rusqlite::Result<u64> {
    let n = conn.execute(
        "DELETE FROM github_contributions WHERE connector_id = ?1 AND kind != 'pr'",
        params![connector_id],
    )?;
    Ok(n as u64)
}

/// Free-text search over title + repo + body for the AI
/// `search_changelog` tool. `query` empty → most-recent. PR-only;
/// `merged_only` narrows to delivered features.
pub fn search_contributions(
    conn: &Connection,
    query: &str,
    merged_only: bool,
    limit: usize,
) -> rusqlite::Result<Vec<Contribution>> {
    let mut sql = format!("SELECT {SELECT_COLS} FROM github_contributions WHERE kind = 'pr'");
    if !query.trim().is_empty() {
        sql.push_str(" AND (title LIKE ?1 OR repo LIKE ?1 OR body LIKE ?1)");
    }
    if merged_only {
        sql.push_str(" AND state = 'merged'");
    }
    sql.push_str(" ORDER BY COALESCE(merged_at_ms, created_at_ms) DESC LIMIT ?2");

    let like = format!("%{}%", query.trim().replace('%', "\\%").replace('_', "\\_"));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![like, limit as i64], row_to_contribution)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// True when at least one row exists — drives the changelog empty state.
#[allow(dead_code)]
pub fn count_contributions(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM github_contributions", [], |r| r.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        conn.execute(
            "INSERT INTO connectors(id, kind, display_name, enabled, config_json, created_ms, updated_ms) \
             VALUES ('github:octocat', 'github', 'GitHub (octocat)', 1, '{}', 0, 0)",
            [],
        )
        .unwrap();
        // Seed the self team member so event emission's FK holds.
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, is_self, created_ms, updated_ms) \
             VALUES ('me', 'Me', '', 1, 0, 0)",
            [],
        )
        .unwrap();
        conn
    }

    fn mk(id: &str, kind: &str, state: &str, created: i64, merged: Option<i64>) -> Contribution {
        Contribution {
            id: format!("github:octocat::{id}"),
            connector_id: "github:octocat".into(),
            external_id: id.into(),
            kind: kind.into(),
            state: state.into(),
            title: format!("title {id}"),
            body: Some("body".into()),
            repo: "octocat/hello".into(),
            url: format!("https://github.com/octocat/hello/{id}"),
            author_login: "octocat".into(),
            author_avatar_url: None,
            created_at_ms: created,
            merged_at_ms: merged,
            modified_ms: created,
            ai_summary: None,
            ai_highlight: None,
            ai_generated_ms: None,
        }
    }

    #[test]
    fn upsert_counts_added_then_updated() {
        let mut conn = open_db();
        let items = vec![
            mk("pr:octocat/hello#1", "pr", "open", 1_000, None),
            mk("commit:abc", "commit", "committed", 2_000, None),
        ];
        let r1 = upsert_contributions(&mut conn, "github:octocat", &items).unwrap();
        assert_eq!(r1.added, 2);
        assert_eq!(r1.updated, 0);

        // Re-sync with the PR now merged — same id, in-place update.
        let mut merged = items.clone();
        merged[0].state = "merged".into();
        merged[0].merged_at_ms = Some(3_000);
        let r2 = upsert_contributions(&mut conn, "github:octocat", &merged).unwrap();
        assert_eq!(r2.added, 0);
        assert_eq!(r2.updated, 2);

        let state: String = conn
            .query_row(
                "SELECT state FROM github_contributions WHERE external_id = 'pr:octocat/hello#1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "merged");
    }

    #[test]
    fn list_is_pr_only_and_orders_by_effective_timestamp() {
        let mut conn = open_db();
        upsert_contributions(
            &mut conn,
            "github:octocat",
            &[
                mk("commit:a", "commit", "committed", 5_000, None),
                mk("pr:r#1", "pr", "merged", 1_000, Some(9_000)),
                mk("pr:r#2", "pr", "open", 7_000, None),
            ],
        )
        .unwrap();
        let all = list_contributions(&conn, 10).unwrap();
        // Commits excluded; PR merged at 9_000 sorts ahead of open PR at 7_000.
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].external_id, "pr:r#1");
        assert_eq!(all[1].external_id, "pr:r#2");
    }

    #[test]
    fn prune_removes_commits_only() {
        let mut conn = open_db();
        upsert_contributions(
            &mut conn,
            "github:octocat",
            &[
                mk("commit:a", "commit", "committed", 5_000, None),
                mk("pr:r#1", "pr", "merged", 1_000, Some(9_000)),
            ],
        )
        .unwrap();
        let removed = prune_non_prs(&conn, "github:octocat").unwrap();
        assert_eq!(removed, 1);
        assert_eq!(list_contributions(&conn, 10).unwrap().len(), 1);
    }

    #[test]
    fn search_filters_by_query_and_merged() {
        let mut conn = open_db();
        upsert_contributions(
            &mut conn,
            "github:octocat",
            &[
                mk("pr:r#1", "pr", "merged", 1_000, Some(2_000)),
                mk("pr:r#2", "pr", "open", 1_500, None),
            ],
        )
        .unwrap();
        let merged = search_contributions(&conn, "title", true, 10).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].external_id, "pr:r#1");
    }

    #[test]
    fn set_and_read_ai_insight() {
        let mut conn = open_db();
        upsert_contributions(
            &mut conn,
            "github:octocat",
            &[mk("pr:r#1", "pr", "merged", 1_000, Some(2_000))],
        )
        .unwrap();
        let id = "github:octocat::pr:r#1";
        set_ai_insight(&conn, id, "Did a thing.", Some("{\"angle\":\"a\",\"content\":\"c\"}"), 42)
            .unwrap();
        let c = get_contribution(&conn, id).unwrap().unwrap();
        assert_eq!(c.ai_summary.as_deref(), Some("Did a thing."));
        assert_eq!(c.ai_generated_ms, Some(42));
        assert!(c.ai_highlight.unwrap().contains("angle"));
    }
}
