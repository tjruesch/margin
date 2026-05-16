//! Provider-agnostic Teams messages storage layer (#105).
//!
//! Mirrors `connectors/email.rs`. The `microsoft_graph` connector
//! maps Graph chat messages onto `TeamsMessage` + `TeamsChatMember`
//! and calls `upsert_messages` / `upsert_chat_members` to persist
//! into the shared `teams_messages` / `teams_chat_members` tables.
//!
//! Like email, this layer is accumulate-only — no automatic pruning.
//! Old messages stay until an explicit retention policy lands. The
//! synthesizer can still surface threads from months ago via the
//! workstream signal source.
//!
//! `body_html` follows the same lazy-fetch pattern as email: not
//! always populated at sync time; preserved across upserts via
//! `COALESCE(excluded.body_html, teams_messages.body_html)`.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct TeamsMessage {
    pub id: String,
    pub connector_id: String,
    pub external_id: String,
    pub chat_id: String,
    /// "oneOnOne" | "group" | "meeting"  (channel excluded in v1)
    pub chat_kind: String,
    pub chat_topic: Option<String>,
    pub sent_at_ms: i64,
    pub from_aad_id: Option<String>,
    pub from_email: Option<String>,
    pub from_name: Option<String>,
    pub body_html: Option<String>,
    pub body_preview: Option<String>,
    pub reply_to_id: Option<String>,
    pub modified_ms: i64,
    pub raw_etag: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TeamsChatMember {
    pub chat_id: String,
    pub aad_id: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub team_member_id: Option<String>,
    pub is_self: bool,
}

#[derive(Debug, Default, Clone)]
pub struct UpsertReport {
    pub added: u64,
    pub updated: u64,
    pub skipped: u64,
}

/// Insert or update each message in one transaction. Returns
/// `added` / `updated` / `skipped` counts. Defensive: messages with
/// no `sent_at_ms` get skipped rather than crashing the sync.
pub fn upsert_messages(
    conn: &mut Connection,
    messages: &[TeamsMessage],
) -> rusqlite::Result<UpsertReport> {
    let tx = conn.transaction()?;
    let mut report = UpsertReport::default();
    for m in messages {
        if m.id.is_empty() || m.chat_id.is_empty() {
            report.skipped += 1;
            continue;
        }
        let pre_existed: i64 = tx
            .query_row(
                "SELECT 1 FROM teams_messages WHERE id = ?1",
                params![m.id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        upsert_message(&tx, m)?;
        if pre_existed != 0 {
            report.updated += 1;
        } else {
            // Live event emission (#106). Actor resolution: the chat
            // membership table is upserted *before* messages by
            // microsoft_graph::sync_teams, so the team_member_id is
            // already populated when we get here.
            let actor_id: Option<String> = match &m.from_aad_id {
                Some(aad) => tx
                    .query_row(
                        "SELECT team_member_id FROM teams_chat_members \
                         WHERE chat_id = ?1 AND aad_id = ?2",
                        params![&m.chat_id, aad],
                        |r| r.get::<_, Option<String>>(0),
                    )
                    .optional()?
                    .flatten(),
                None => None,
            };
            let payload = serde_json::json!({
                "chat_kind": m.chat_kind,
                "chat_topic": m.chat_topic,
            });
            crate::events::emit(
                &tx,
                m.sent_at_ms,
                "message_sent",
                actor_id.as_deref(),
                "teams_message",
                &m.id,
                &payload,
            )?;
            // When self sends, mark every other team_member in the chat
            // dirty so their profile worker picks them up on the next
            // tick (#121). Without this the waiting-action surface
            // depends on the 24h TTL to clear stale entries — replies
            // would otherwise sit visible until tomorrow.
            let actor_is_self: bool = match actor_id.as_deref() {
                Some(id) => tx
                    .query_row(
                        "SELECT is_self FROM team_members WHERE id = ?1",
                        params![id],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap_or(0)
                    != 0,
                None => false,
            };
            if actor_is_self {
                let cp_ids: Vec<String> = {
                    let mut cp_stmt = tx.prepare(
                        "SELECT DISTINCT team_member_id FROM teams_chat_members \
                          WHERE chat_id = ?1 \
                            AND is_self = 0 \
                            AND team_member_id IS NOT NULL",
                    )?;
                    let rows = cp_stmt
                        .query_map(params![&m.chat_id], |r| r.get::<_, String>(0))?;
                    rows.filter_map(Result::ok).collect()
                };
                for cp_id in cp_ids {
                    crate::events::emit(
                        &tx,
                        m.sent_at_ms,
                        "counterparty_replied",
                        Some(&cp_id),
                        "teams_message",
                        &m.id,
                        &serde_json::json!({ "source": "teams" }),
                    )?;
                }
            }
            report.added += 1;
        }
    }
    tx.commit()?;
    Ok(report)
}

fn upsert_message(
    tx: &rusqlite::Transaction<'_>,
    m: &TeamsMessage,
) -> rusqlite::Result<()> {
    // Mirrors email.rs upsert. `body_html` preserved via COALESCE.
    tx.execute(
        "INSERT INTO teams_messages(\
            id, connector_id, external_id, chat_id, chat_kind, chat_topic, \
            sent_at_ms, from_aad_id, from_email, from_name, \
            body_html, body_preview, reply_to_id, modified_ms, raw_etag\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15) \
         ON CONFLICT(id) DO UPDATE SET \
            chat_id = excluded.chat_id, \
            chat_kind = excluded.chat_kind, \
            chat_topic = excluded.chat_topic, \
            sent_at_ms = excluded.sent_at_ms, \
            from_aad_id = excluded.from_aad_id, \
            from_email = excluded.from_email, \
            from_name = excluded.from_name, \
            body_html = COALESCE(excluded.body_html, teams_messages.body_html), \
            body_preview = excluded.body_preview, \
            reply_to_id = excluded.reply_to_id, \
            modified_ms = excluded.modified_ms, \
            raw_etag = excluded.raw_etag",
        params![
            m.id,
            m.connector_id,
            m.external_id,
            m.chat_id,
            m.chat_kind,
            m.chat_topic,
            m.sent_at_ms,
            m.from_aad_id,
            m.from_email,
            m.from_name,
            m.body_html,
            m.body_preview,
            m.reply_to_id,
            m.modified_ms,
            m.raw_etag,
        ],
    )?;
    Ok(())
}

pub fn upsert_chat_members(
    conn: &mut Connection,
    chat_id: &str,
    members: &[TeamsChatMember],
) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    // Replace this chat's membership wholesale — members leave/join
    // chats and we want the latest snapshot.
    tx.execute(
        "DELETE FROM teams_chat_members WHERE chat_id = ?1",
        params![chat_id],
    )?;
    let mut stmt = tx.prepare_cached(
        "INSERT INTO teams_chat_members(\
            chat_id, aad_id, email, display_name, team_member_id, is_self\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for m in members {
        if m.aad_id.is_empty() {
            continue;
        }
        stmt.execute(params![
            m.chat_id,
            m.aad_id,
            m.email,
            m.display_name,
            m.team_member_id,
            m.is_self as i64,
        ])?;
    }
    drop(stmt);
    tx.commit()?;
    Ok(())
}

/// Most-recent-first listing of messages whose `sent_at_ms` falls in
/// `[sent_from_ms, sent_to_ms]`. Used by the workstream synth's
/// signal source.
pub fn list_messages_in_range(
    conn: &Connection,
    sent_from_ms: i64,
    sent_to_ms: i64,
    limit: usize,
) -> rusqlite::Result<Vec<TeamsMessage>> {
    let mut stmt = conn.prepare(
        "SELECT id, connector_id, external_id, chat_id, chat_kind, chat_topic, \
                sent_at_ms, from_aad_id, from_email, from_name, \
                body_html, body_preview, reply_to_id, modified_ms, raw_etag \
         FROM teams_messages \
         WHERE sent_at_ms BETWEEN ?1 AND ?2 \
         ORDER BY sent_at_ms DESC \
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![sent_from_ms, sent_to_ms, limit as i64],
        row_to_message,
    )?;
    rows.collect::<Result<Vec<_>, _>>()
}

/// Hydrate a batch of message ids in one query. Used by the
/// synthesizer when expanding workstream-attached message labels.
pub fn get_message_details_batch(
    conn: &Connection,
    ids: &[String],
) -> rusqlite::Result<Vec<TeamsMessage>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders: String = ids.iter().enumerate().map(|(i, _)| format!("?{}", i + 1)).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT id, connector_id, external_id, chat_id, chat_kind, chat_topic, \
                sent_at_ms, from_aad_id, from_email, from_name, \
                body_html, body_preview, reply_to_id, modified_ms, raw_etag \
         FROM teams_messages WHERE id IN ({placeholders}) \
         ORDER BY sent_at_ms DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> =
        ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(rusqlite::params_from_iter(params), row_to_message)?;
    rows.collect::<Result<Vec<_>, _>>()
}

/// Fetch the messages immediately around `around_sent_at_ms` in the
/// same chat, for conversational context in `read_teams_message` (#136).
/// Returns `(before, after)` where:
///   - `before` is DESC by sent_at_ms (newest of the older messages
///     first), capped at `before_n`.
///   - `after` is ASC by sent_at_ms (oldest of the newer messages
///     first), capped at `after_n`.
/// Excludes the anchor row itself.
pub fn list_chat_context(
    conn: &Connection,
    chat_id: &str,
    around_sent_at_ms: i64,
    before_n: usize,
    after_n: usize,
) -> rusqlite::Result<(Vec<TeamsMessage>, Vec<TeamsMessage>)> {
    let select_cols = "id, connector_id, external_id, chat_id, chat_kind, chat_topic, \
                       sent_at_ms, from_aad_id, from_email, from_name, \
                       body_html, body_preview, reply_to_id, modified_ms, raw_etag";
    let before = {
        let sql = format!(
            "SELECT {cols} FROM teams_messages \
              WHERE chat_id = ?1 AND sent_at_ms < ?2 \
              ORDER BY sent_at_ms DESC LIMIT ?3",
            cols = select_cols,
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![chat_id, around_sent_at_ms, before_n as i64],
            row_to_message,
        )?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let after = {
        let sql = format!(
            "SELECT {cols} FROM teams_messages \
              WHERE chat_id = ?1 AND sent_at_ms > ?2 \
              ORDER BY sent_at_ms ASC LIMIT ?3",
            cols = select_cols,
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![chat_id, around_sent_at_ms, after_n as i64],
            row_to_message,
        )?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    Ok((before, after))
}

fn row_to_message(r: &rusqlite::Row<'_>) -> rusqlite::Result<TeamsMessage> {
    Ok(TeamsMessage {
        id: r.get(0)?,
        connector_id: r.get(1)?,
        external_id: r.get(2)?,
        chat_id: r.get(3)?,
        chat_kind: r.get(4)?,
        chat_topic: r.get(5)?,
        sent_at_ms: r.get(6)?,
        from_aad_id: r.get(7)?,
        from_email: r.get(8)?,
        from_name: r.get(9)?,
        body_html: r.get(10)?,
        body_preview: r.get(11)?,
        reply_to_id: r.get(12)?,
        modified_ms: r.get(13)?,
        raw_etag: r.get(14)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        // Seed a connector row so the FK on teams_messages.connector_id
        // is satisfied.
        conn.execute(
            "INSERT INTO connectors(id, kind, display_name, enabled, config_json, created_ms, updated_ms) \
             VALUES ('microsoft_graph:test@x.io', 'microsoft_graph', 'Test', 1, '{}', 0, 0)",
            [],
        )
        .unwrap();
        conn
    }

    fn sample_message(id: &str, chat_id: &str, sent_at: i64, body: &str) -> TeamsMessage {
        TeamsMessage {
            id: id.into(),
            connector_id: "microsoft_graph:test@x.io".into(),
            external_id: id.replace("microsoft_graph:test@x.io::teams::", ""),
            chat_id: chat_id.into(),
            chat_kind: "oneOnOne".into(),
            chat_topic: None,
            sent_at_ms: sent_at,
            from_aad_id: Some("aad-1".into()),
            from_email: Some("alice@x.io".into()),
            from_name: Some("Alice".into()),
            body_html: Some(format!("<p>{body}</p>")),
            body_preview: Some(body.into()),
            reply_to_id: None,
            modified_ms: sent_at,
            raw_etag: None,
        }
    }

    #[test]
    fn upsert_messages_emits_event_per_new_row() {
        let mut conn = open_db();
        let m = sample_message(
            "microsoft_graph:test@x.io::teams::evt1",
            "chat-1",
            1_000,
            "hi",
        );
        upsert_messages(&mut conn, &[m.clone()]).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE ref_kind = 'teams_message'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);

        // Re-upsert same row: no new event.
        upsert_messages(&mut conn, &[m]).unwrap();
        let n2: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE ref_kind = 'teams_message'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n2, 1);
    }

    #[test]
    fn upsert_round_trip() {
        let mut conn = open_db();
        let msg = sample_message(
            "microsoft_graph:test@x.io::teams::m1",
            "chat-1",
            1_000,
            "hi",
        );
        let r = upsert_messages(&mut conn, &[msg.clone()]).unwrap();
        assert_eq!(r.added, 1);
        assert_eq!(r.updated, 0);

        let details = get_message_details_batch(&conn, &[msg.id.clone()]).unwrap();
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].chat_id, "chat-1");
        assert_eq!(details[0].body_preview.as_deref(), Some("hi"));
    }

    #[test]
    fn upsert_preserves_body_html_on_resync() {
        let mut conn = open_db();
        let mut msg = sample_message(
            "microsoft_graph:test@x.io::teams::m1",
            "chat-1",
            1_000,
            "hi",
        );
        msg.body_html = Some("<p>full body</p>".into());
        upsert_messages(&mut conn, &[msg.clone()]).unwrap();

        // Re-sync without body_html — must NOT clobber the existing value.
        msg.body_html = None;
        msg.modified_ms = 2_000;
        upsert_messages(&mut conn, &[msg.clone()]).unwrap();

        let body: Option<String> = conn
            .query_row(
                "SELECT body_html FROM teams_messages WHERE id = ?1",
                params![&msg.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(body.as_deref(), Some("<p>full body</p>"));
    }

    #[test]
    fn upsert_chat_members_replaces_set() {
        let mut conn = open_db();
        upsert_chat_members(
            &mut conn,
            "chat-1",
            &[
                TeamsChatMember {
                    chat_id: "chat-1".into(),
                    aad_id: "aad-1".into(),
                    email: Some("alice@x.io".into()),
                    display_name: Some("Alice".into()),
                    team_member_id: None,
                    is_self: false,
                },
                TeamsChatMember {
                    chat_id: "chat-1".into(),
                    aad_id: "aad-self".into(),
                    email: Some("me@x.io".into()),
                    display_name: Some("Me".into()),
                    team_member_id: None,
                    is_self: true,
                },
            ],
        )
        .unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM teams_chat_members WHERE chat_id = 'chat-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);

        // Replace with a smaller set — old members should drop.
        upsert_chat_members(
            &mut conn,
            "chat-1",
            &[TeamsChatMember {
                chat_id: "chat-1".into(),
                aad_id: "aad-1".into(),
                email: Some("alice@x.io".into()),
                display_name: Some("Alice".into()),
                team_member_id: None,
                is_self: false,
            }],
        )
        .unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM teams_chat_members WHERE chat_id = 'chat-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Seed a team_members row. Post-migration schema (after #017 +
    /// #117) is (id, display_name, role, is_self, created_ms, updated_ms).
    fn seed_member(conn: &Connection, id: &str, name: &str, is_self: bool) {
        conn.execute(
            "INSERT INTO team_members(\
                id, display_name, role, \
                is_self, created_ms, updated_ms\
             ) VALUES (?1, ?2, '', ?3, 0, 0)",
            params![id, name, is_self as i64],
        )
        .unwrap();
    }

    fn cp_count(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM events \
              WHERE kind = 'counterparty_replied' AND ref_kind = 'teams_message'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Self-sent message in a chat with self + Alice + Bob emits one
    /// `counterparty_replied` per non-self member (#121).
    #[test]
    fn self_sent_emits_counterparty_replied_per_other_member() {
        let mut conn = open_db();
        seed_member(&conn, "tm:self", "Me", true);
        seed_member(&conn, "tm:alice", "Alice", false);
        seed_member(&conn, "tm:bob", "Bob", false);
        upsert_chat_members(
            &mut conn,
            "chat-1",
            &[
                TeamsChatMember {
                    chat_id: "chat-1".into(),
                    aad_id: "aad-self".into(),
                    email: Some("me@x.io".into()),
                    display_name: Some("Me".into()),
                    team_member_id: Some("tm:self".into()),
                    is_self: true,
                },
                TeamsChatMember {
                    chat_id: "chat-1".into(),
                    aad_id: "aad-alice".into(),
                    email: Some("alice@x.io".into()),
                    display_name: Some("Alice".into()),
                    team_member_id: Some("tm:alice".into()),
                    is_self: false,
                },
                TeamsChatMember {
                    chat_id: "chat-1".into(),
                    aad_id: "aad-bob".into(),
                    email: Some("bob@x.io".into()),
                    display_name: Some("Bob".into()),
                    team_member_id: Some("tm:bob".into()),
                    is_self: false,
                },
            ],
        )
        .unwrap();
        let mut m = sample_message(
            "microsoft_graph:test@x.io::teams::m-self",
            "chat-1",
            1_000,
            "ping",
        );
        m.from_aad_id = Some("aad-self".into());
        upsert_messages(&mut conn, &[m]).unwrap();

        let mut actors: Vec<String> = conn
            .prepare(
                "SELECT actor_id FROM events \
                  WHERE kind = 'counterparty_replied' AND ref_kind = 'teams_message'",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        actors.sort();
        assert_eq!(actors, vec!["tm:alice".to_string(), "tm:bob".to_string()]);
    }

    /// Inbound message (Alice → self) must not emit any
    /// `counterparty_replied` rows — only outbound self-actions
    /// dirty counterparties.
    #[test]
    fn inbound_message_does_not_emit_counterparty_replied() {
        let mut conn = open_db();
        seed_member(&conn, "tm:self", "Me", true);
        seed_member(&conn, "tm:alice", "Alice", false);
        upsert_chat_members(
            &mut conn,
            "chat-1",
            &[
                TeamsChatMember {
                    chat_id: "chat-1".into(),
                    aad_id: "aad-self".into(),
                    email: Some("me@x.io".into()),
                    display_name: Some("Me".into()),
                    team_member_id: Some("tm:self".into()),
                    is_self: true,
                },
                TeamsChatMember {
                    chat_id: "chat-1".into(),
                    aad_id: "aad-alice".into(),
                    email: Some("alice@x.io".into()),
                    display_name: Some("Alice".into()),
                    team_member_id: Some("tm:alice".into()),
                    is_self: false,
                },
            ],
        )
        .unwrap();
        let mut m = sample_message(
            "microsoft_graph:test@x.io::teams::m-in",
            "chat-1",
            1_000,
            "hi",
        );
        m.from_aad_id = Some("aad-alice".into());
        upsert_messages(&mut conn, &[m]).unwrap();
        assert_eq!(cp_count(&conn), 0);
    }

    /// Re-upserting the same self-sent message must not duplicate
    /// `counterparty_replied` rows — emission is gated on
    /// `pre_existed == 0` like the parent `message_sent` event.
    #[test]
    fn pre_existing_message_does_not_re_emit() {
        let mut conn = open_db();
        seed_member(&conn, "tm:self", "Me", true);
        seed_member(&conn, "tm:alice", "Alice", false);
        upsert_chat_members(
            &mut conn,
            "chat-1",
            &[
                TeamsChatMember {
                    chat_id: "chat-1".into(),
                    aad_id: "aad-self".into(),
                    email: Some("me@x.io".into()),
                    display_name: Some("Me".into()),
                    team_member_id: Some("tm:self".into()),
                    is_self: true,
                },
                TeamsChatMember {
                    chat_id: "chat-1".into(),
                    aad_id: "aad-alice".into(),
                    email: Some("alice@x.io".into()),
                    display_name: Some("Alice".into()),
                    team_member_id: Some("tm:alice".into()),
                    is_self: false,
                },
            ],
        )
        .unwrap();
        let mut m = sample_message(
            "microsoft_graph:test@x.io::teams::m-once",
            "chat-1",
            1_000,
            "again",
        );
        m.from_aad_id = Some("aad-self".into());
        upsert_messages(&mut conn, &[m.clone()]).unwrap();
        upsert_messages(&mut conn, &[m]).unwrap();
        assert_eq!(cp_count(&conn), 1, "second upsert must not re-emit");
    }

    #[test]
    fn list_messages_in_range_respects_window() {
        let mut conn = open_db();
        upsert_messages(
            &mut conn,
            &[
                sample_message("microsoft_graph:test@x.io::teams::m1", "chat-1", 1_000, "a"),
                sample_message("microsoft_graph:test@x.io::teams::m2", "chat-1", 5_000, "b"),
                sample_message("microsoft_graph:test@x.io::teams::m3", "chat-1", 9_000, "c"),
            ],
        )
        .unwrap();
        let got = list_messages_in_range(&conn, 2_000, 8_000, 100).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body_preview.as_deref(), Some("b"));
    }
}
