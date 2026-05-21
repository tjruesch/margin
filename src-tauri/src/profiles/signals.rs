//! Waiting-direction signal mining for the v3 profile worker (#120).
//!
//! The hybrid: deterministic SQL surfaces candidate "waiting" items
//! per direction from email_messages, teams_messages, and
//! calendar_events; the worker prompt hands them to Claude, which
//! filters out resolved/stale ones, rephrases the preview into a
//! one-sentence description, and emits the most consequential as
//! `WaitingItem`s on the snapshot body.
//!
//! Conventions:
//!   - **from_me** = items the team member is waiting on the user for
//!                   (you owe them).
//!   - **for_them** = items the user is waiting on the team member for
//!                    (they owe you).
//!
//! All queries are recency-windowed (`RECENCY_WINDOW_MS`) and capped
//! per direction (`CANDIDATES_PER_DIRECTION_CAP`). Meeting candidates
//! are sub-capped so they can't crowd out higher-signal email/Teams
//! items.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

pub const RECENCY_WINDOW_MS: i64 = 30 * 24 * 3_600 * 1_000;
pub const CANDIDATES_PER_DIRECTION_CAP: usize = 20;
pub const MEETING_SUBCAP: usize = 5;
pub const TAIL_CAP: usize = 5;
pub const TAIL_PREVIEW_CHARS: usize = 200;

#[derive(Serialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct WaitingCandidate {
    pub source_kind: String,
    pub source_ref_id: String,
    pub since_ms: i64,
    pub preview: String,
    /// Up to 5 follow-up messages from the same thread/chat, ordered
    /// oldest-first. Each line lets the LLM judge whether the ask is
    /// pending, resolved, or committed-not-delivered. Empty for
    /// meeting candidates (their "tail" is the meeting note, which
    /// we surface via the note-linked-action subsystem).
    pub conversation_tail: Vec<TailEntry>,
    /// Populated by `hydrate_chat_participants` for Teams candidates
    /// in chats with > 2 distinct members (#125). Empty for one-on-one
    /// Teams chats (the question is implicitly addressed to the user)
    /// and for email/meeting candidates (their recipient context is
    /// explicit elsewhere). `skip_serializing_if` keeps the JSON
    /// payload size flat for 1:1 chats.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chat_participants: Vec<ChatParticipant>,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct TailEntry {
    pub ms: i64,
    pub from_kind: String, // "self" | "them"
    pub preview: String,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ChatParticipant {
    pub display_name: String,
    pub is_self: bool,
}

/// Drop a candidate when its preview is a social ack or sign-off
/// followed by signature material — those messages shouldn't bother
/// the LLM. The rules:
///
/// 1. A `?` anywhere in the preview means it's a real ask → keep.
/// 2. Take the first phrase (split on `.` `!` `\n`). Strip a known
///    ack token (Thank you / Danke / OK / etc.) from its start.
///    a. If the result is empty AND every later phrase looks like
///       signature material → drop.
///    b. If the result is empty but a later phrase has real prose →
///       keep (e.g. "Thanks! Btw, can you confirm?").
/// 3. Otherwise, keep if total alphanumeric content ≥ 30 chars.
fn is_substantive_preview(preview: &str) -> bool {
    if preview.contains('?') {
        return true;
    }
    let lowered = preview.to_ascii_lowercase();
    let mut phrases = lowered.split(|c: char| matches!(c, '.' | '!' | '\n'));
    let first = phrases.next().unwrap_or("").trim();
    let after_ack = strip_leading_ack(first).trim();

    if after_ack.is_empty() {
        // First phrase was a pure ack. Keep only if a *later* phrase
        // carries real substance (not signature lines).
        return phrases.any(|p| {
            let pt = p.trim();
            !pt.is_empty()
                && !looks_like_signature_fragment(pt)
                && pt.chars().filter(|c| c.is_alphanumeric()).count() >= 20
        });
    }

    // First phrase has content. Use overall length as the cheap signal.
    let alnum: usize = lowered.chars().filter(|c| c.is_alphanumeric()).count();
    alnum >= 30
}

/// True when a phrase looks like email-signature scaffolding (job
/// title / company / contact info), not a real conversational line.
fn looks_like_signature_fragment(phrase: &str) -> bool {
    static SIG_TOKENS: &[&str] = &[
        "manager",
        "director",
        " lead",
        " ceo",
        " cto",
        " cfo",
        " vp ",
        "engineer",
        "developer",
        "consultant",
        "elanlanguages",
        "elan languages",
        "regards",
        "grüße",
        "freundlich",
        "tel:",
        "mob:",
        "phone:",
        "http://",
        "https://",
        "www.",
    ];
    SIG_TOKENS.iter().any(|t| phrase.contains(t))
}

fn strip_leading_ack(s: &str) -> &str {
    let trimmed = s.trim_start();
    // Order matters: longest prefixes first so "vielen dank" doesn't
    // get short-circuited by "danke".
    const ACK_PREFIXES: &[&str] = &[
        "vielen dank",
        "thank you",
        "thanks",
        "danke schön",
        "danke dir",
        "danke",
        "sounds good",
        "got it",
        "merci",
        "cheers",
        "perfect",
        "noted",
        "agreed",
        "alright",
        "super",
        "great",
        "okay",
        "ok",
    ];
    for prefix in ACK_PREFIXES {
        if trimmed.starts_with(prefix) {
            let rest = &trimmed[prefix.len()..];
            return rest
                .trim_start_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace());
        }
    }
    trimmed
}

/// Items the team member is waiting on the user to act on. Combines
/// inbound email candidates, inbound Teams candidates, and past
/// meetings that lack a note. Sorted by recency desc, capped.
/// Conversation tails are hydrated per candidate so the LLM can judge
/// resolution status from the reply chain.
pub fn candidates_from_me(
    conn: &Connection,
    person_id: &str,
    now_ms: i64,
) -> rusqlite::Result<Vec<WaitingCandidate>> {
    let cutoff = now_ms - RECENCY_WINDOW_MS;
    let mut out = Vec::new();
    out.extend(inbound_email(conn, person_id, cutoff)?);
    out.extend(inbound_teams(conn, person_id, cutoff)?);
    out.extend(meeting_past_without_note(
        conn, person_id, cutoff, now_ms, MEETING_SUBCAP,
    )?);
    sort_and_cap(&mut out, CANDIDATES_PER_DIRECTION_CAP);
    hydrate_tails(conn, &mut out)?;
    hydrate_chat_participants(conn, &mut out)?;
    Ok(out)
}

/// Items the user is waiting on the team member to act on. Combines
/// outbound email candidates, outbound Teams candidates, and future
/// meetings the member organized that the user hasn't accepted.
pub fn candidates_for_them(
    conn: &Connection,
    person_id: &str,
    now_ms: i64,
) -> rusqlite::Result<Vec<WaitingCandidate>> {
    let cutoff = now_ms - RECENCY_WINDOW_MS;
    let mut out = Vec::new();
    out.extend(outbound_email(conn, person_id, cutoff)?);
    out.extend(outbound_teams(conn, person_id, cutoff)?);
    out.extend(meeting_future_unaccepted(
        conn, person_id, now_ms, MEETING_SUBCAP,
    )?);
    sort_and_cap(&mut out, CANDIDATES_PER_DIRECTION_CAP);
    hydrate_tails(conn, &mut out)?;
    hydrate_chat_participants(conn, &mut out)?;
    Ok(out)
}

fn sort_and_cap(items: &mut Vec<WaitingCandidate>, cap: usize) {
    items.sort_by(|a, b| b.since_ms.cmp(&a.since_ms));
    items.truncate(cap);
}

/// Per-source-kind cap on rejected-waiting items fed to the worker
/// prompt (#149). Recent first; older rows drop.
pub const REJECTED_WAITING_PER_KIND_CAP: usize = 10;

/// Build the per-person "recently rejected waiting actions" payload
/// for the worker prompt (#149). Returns a JSON object keyed by short
/// source_kind (`email` / `teams` / `meeting`); each value is an array
/// of action-item text strings the user previously deleted or
/// dismissed *about this person*, last 30 days.
///
/// Matches the person either via the action's counterparty
/// (`subject_member_id = person_id`, which covers `waiting_from_me`
/// rows where the user was assignee) OR via the assignee
/// (`assignee_id = person_id`, which covers `waiting_for_them` rows
/// where the user was waiting on them). Either direction is the same
/// "we tried to surface a waiting action about this person and the
/// user rejected it" signal.
///
/// Cause filter: `user_delete` / `user_dismiss` only. `auto_resolved`
/// is excluded — worker omissions are weak signal; including them
/// would create a feedback loop where the LLM hides items so the
/// worker sweeps them, training the LLM to hide more.
///
/// Returns `None` (and the caller omits the key) when no kind has any
/// matching rows — keeps the prompt clean for the steady state.
pub fn recently_rejected_waiting(
    conn: &Connection,
    person_id: &str,
    now_ms: i64,
) -> rusqlite::Result<Option<serde_json::Value>> {
    let cutoff = now_ms - RECENCY_WINDOW_MS;
    const KINDS: &[(&str, &str)] = &[
        ("email_waiting", "email"),
        ("teams_waiting", "teams"),
        ("meeting_waiting", "meeting"),
    ];

    let mut out = serde_json::Map::new();
    for (synth_kind, short_kind) in KINDS {
        let mut stmt = conn.prepare(
            "SELECT text \
               FROM action_deletions \
              WHERE origin_synth_kind = ?1 \
                AND (subject_member_id = ?2 OR assignee_id = ?2) \
                AND cause IN ('user_delete', 'user_dismiss') \
                AND deleted_ms > ?3 \
              ORDER BY deleted_ms DESC \
              LIMIT ?4",
        )?;
        let texts: Vec<String> = stmt
            .query_map(
                params![synth_kind, person_id, cutoff, REJECTED_WAITING_PER_KIND_CAP as i64],
                |r| r.get::<_, String>(0),
            )?
            .filter_map(Result::ok)
            .collect();
        if !texts.is_empty() {
            out.insert(
                (*short_kind).to_string(),
                serde_json::Value::Array(
                    texts.into_iter().map(serde_json::Value::String).collect(),
                ),
            );
        }
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::Value::Object(out)))
    }
}

// ---------- Email ---------------------------------------------------------

/// Person → self emails within the recency window. The previous
/// "NOT EXISTS (later self reply)" filter is gone — resolution is
/// now an LLM judgment based on the hydrated `conversation_tail`.
/// Uses the `self_alias_emails` CTE pattern from `activity.rs` to
/// identify the user across address forms.
fn inbound_email(
    conn: &Connection,
    person_id: &str,
    cutoff_ms: i64,
) -> rusqlite::Result<Vec<WaitingCandidate>> {
    let sql = "\
        WITH self_emails AS ( \
            SELECT lower(a.value) AS email FROM team_member_aliases a \
              JOIN team_members m ON m.id = a.member_id \
             WHERE a.kind = 'email' AND m.is_self = 1 \
        ), \
        their_emails AS ( \
            SELECT lower(a.value) AS email FROM team_member_aliases a \
             WHERE a.kind = 'email' AND a.member_id = ?1 \
        ) \
        SELECT em.id, em.sent_at_ms, \
               COALESCE(NULLIF(em.body_preview, ''), em.subject, '') AS preview \
          FROM email_messages em \
         WHERE em.sent_at_ms >= ?2 \
           AND lower(em.from_email) IN (SELECT email FROM their_emails) \
           AND EXISTS ( \
                SELECT 1 FROM email_recipients er \
                 WHERE er.message_id = em.id \
                   AND er.recipient_type IN ('to', 'cc') \
                   AND ( \
                       er.team_member_id = (SELECT id FROM team_members WHERE is_self = 1 LIMIT 1) \
                       OR lower(er.email) IN (SELECT email FROM self_emails) \
                   ) \
           ) \
         ORDER BY em.sent_at_ms DESC \
         LIMIT ?3";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        params![person_id, cutoff_ms, CANDIDATES_PER_DIRECTION_CAP as i64],
        row_to_email_candidate,
    )?;
    collect_rows(rows)
}

/// Self → person emails within the recency window. Like inbound,
/// the "no reply" SQL filter is gone; the LLM judges resolution
/// from the `conversation_tail`.
fn outbound_email(
    conn: &Connection,
    person_id: &str,
    cutoff_ms: i64,
) -> rusqlite::Result<Vec<WaitingCandidate>> {
    let sql = "\
        WITH self_emails AS ( \
            SELECT lower(a.value) AS email FROM team_member_aliases a \
              JOIN team_members m ON m.id = a.member_id \
             WHERE a.kind = 'email' AND m.is_self = 1 \
        ), \
        their_emails AS ( \
            SELECT lower(a.value) AS email FROM team_member_aliases a \
             WHERE a.kind = 'email' AND a.member_id = ?1 \
        ) \
        SELECT em.id, em.sent_at_ms, \
               COALESCE(NULLIF(em.body_preview, ''), em.subject, '') AS preview \
          FROM email_messages em \
         WHERE em.sent_at_ms >= ?2 \
           AND lower(em.from_email) IN (SELECT email FROM self_emails) \
           AND EXISTS ( \
                SELECT 1 FROM email_recipients er \
                 WHERE er.message_id = em.id \
                   AND er.recipient_type IN ('to', 'cc') \
                   AND ( \
                       er.team_member_id = ?1 \
                       OR lower(er.email) IN (SELECT email FROM their_emails) \
                   ) \
           ) \
         ORDER BY em.sent_at_ms DESC \
         LIMIT ?3";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        params![person_id, cutoff_ms, CANDIDATES_PER_DIRECTION_CAP as i64],
        row_to_email_candidate,
    )?;
    collect_rows(rows)
}

fn row_to_email_candidate(r: &rusqlite::Row<'_>) -> rusqlite::Result<WaitingCandidate> {
    Ok(WaitingCandidate {
        source_kind: "email".into(),
        source_ref_id: r.get(0)?,
        since_ms: r.get(1)?,
        preview: r.get(2)?,
        conversation_tail: Vec::new(),
        chat_participants: Vec::new(),
    })
}

// ---------- Teams ---------------------------------------------------------

/// Person → self Teams messages within the recency window. Matches
/// the person via `teams_chat_members` (both email and aad_id paths)
/// so we catch messages even when `from_email` is NULL. The "no
/// reply" SQL filter is gone; the LLM judges resolution from
/// the `conversation_tail`.
fn inbound_teams(
    conn: &Connection,
    person_id: &str,
    cutoff_ms: i64,
) -> rusqlite::Result<Vec<WaitingCandidate>> {
    let sql = "\
        WITH their_chats AS ( \
            SELECT DISTINCT chat_id FROM teams_chat_members \
             WHERE team_member_id = ?1 \
        ) \
        SELECT tm.id, tm.sent_at_ms, COALESCE(tm.body_preview, '') AS preview \
          FROM teams_messages tm \
          JOIN teams_chat_members tcm \
            ON tcm.chat_id = tm.chat_id AND tcm.team_member_id = ?1 \
         WHERE tm.sent_at_ms >= ?2 \
           AND tm.chat_id IN (SELECT chat_id FROM their_chats) \
           AND ( \
                (tm.from_aad_id IS NOT NULL AND tm.from_aad_id = tcm.aad_id) \
                OR ( \
                    tm.from_email IS NOT NULL \
                    AND tcm.email IS NOT NULL \
                    AND lower(tm.from_email) = lower(tcm.email) \
                ) \
           ) \
         ORDER BY tm.sent_at_ms DESC \
         LIMIT ?3";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        params![person_id, cutoff_ms, CANDIDATES_PER_DIRECTION_CAP as i64],
        row_to_teams_candidate,
    )?;
    collect_rows(rows)
}

/// Self → person Teams messages within the recency window. "No
/// reply" SQL filter dropped; LLM does resolution.
fn outbound_teams(
    conn: &Connection,
    person_id: &str,
    cutoff_ms: i64,
) -> rusqlite::Result<Vec<WaitingCandidate>> {
    let sql = "\
        WITH self_emails AS ( \
            SELECT lower(a.value) AS email FROM team_member_aliases a \
              JOIN team_members m ON m.id = a.member_id \
             WHERE a.kind = 'email' AND m.is_self = 1 \
        ), \
        their_chats AS ( \
            SELECT DISTINCT chat_id FROM teams_chat_members \
             WHERE team_member_id = ?1 \
        ) \
        SELECT tm.id, tm.sent_at_ms, COALESCE(tm.body_preview, '') AS preview \
          FROM teams_messages tm \
          JOIN teams_chat_members scm \
            ON scm.chat_id = tm.chat_id AND scm.is_self = 1 \
         WHERE tm.sent_at_ms >= ?2 \
           AND tm.chat_id IN (SELECT chat_id FROM their_chats) \
           AND ( \
                lower(COALESCE(tm.from_email, '')) IN (SELECT email FROM self_emails) \
                OR (tm.from_aad_id IS NOT NULL AND tm.from_aad_id = scm.aad_id) \
           ) \
         ORDER BY tm.sent_at_ms DESC \
         LIMIT ?3";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        params![person_id, cutoff_ms, CANDIDATES_PER_DIRECTION_CAP as i64],
        row_to_teams_candidate,
    )?;
    collect_rows(rows)
}

fn row_to_teams_candidate(r: &rusqlite::Row<'_>) -> rusqlite::Result<WaitingCandidate> {
    Ok(WaitingCandidate {
        source_kind: "teams".into(),
        source_ref_id: r.get(0)?,
        since_ms: r.get(1)?,
        preview: r.get(2)?,
        conversation_tail: Vec::new(),
        chat_participants: Vec::new(),
    })
}

// ---------- Meetings ------------------------------------------------------

/// Past meeting that included the person and has no linked note.
/// Heuristic: "you owe them a note / write-up."
fn meeting_past_without_note(
    conn: &Connection,
    person_id: &str,
    cutoff_ms: i64,
    now_ms: i64,
    cap: usize,
) -> rusqlite::Result<Vec<WaitingCandidate>> {
    let sql = "\
        SELECT ce.id, ce.start_ms, \
               'Meeting: ' || COALESCE(NULLIF(ce.title, ''), '(untitled)') AS preview \
          FROM calendar_events ce \
          JOIN calendar_attendees ca \
            ON ca.event_id = ce.id AND ca.team_member_id = ?1 \
         WHERE ce.end_ms < ?2 \
           AND ce.end_ms >= ?3 \
           AND ce.linked_note_id IS NULL \
           AND (ce.status IS NULL OR ce.status != 'cancelled') \
         ORDER BY ce.start_ms DESC \
         LIMIT ?4";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        params![person_id, now_ms, cutoff_ms, cap as i64],
        |r| {
            Ok(WaitingCandidate {
                source_kind: "meeting".into(),
                source_ref_id: r.get(0)?,
                since_ms: r.get(1)?,
                preview: r.get(2)?,
                conversation_tail: Vec::new(),
                chat_participants: Vec::new(),
            })
        },
    )?;
    collect_rows(rows)
}

/// Future meeting organized by the person where the user has not
/// accepted or tentatively-accepted yet.
fn meeting_future_unaccepted(
    conn: &Connection,
    person_id: &str,
    now_ms: i64,
    cap: usize,
) -> rusqlite::Result<Vec<WaitingCandidate>> {
    let sql = "\
        SELECT ce.id, ce.start_ms, \
               'Meeting: ' || COALESCE(NULLIF(ce.title, ''), '(untitled)') AS preview \
          FROM calendar_events ce \
          JOIN calendar_attendees co \
            ON co.event_id = ce.id \
           AND co.team_member_id = ?1 \
           AND co.is_organizer = 1 \
          LEFT JOIN calendar_attendees cs \
            ON cs.event_id = ce.id AND cs.is_self = 1 \
         WHERE ce.start_ms >= ?2 \
           AND (ce.status IS NULL OR ce.status != 'cancelled') \
           AND ( \
                cs.response_status IS NULL \
                OR cs.response_status NOT IN ('accepted', 'tentative', 'tentativelyAccepted') \
           ) \
         ORDER BY ce.start_ms ASC \
         LIMIT ?3";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![person_id, now_ms, cap as i64], |r| {
        Ok(WaitingCandidate {
            source_kind: "meeting".into(),
            source_ref_id: r.get(0)?,
            since_ms: r.get(1)?,
            preview: r.get(2)?,
            conversation_tail: Vec::new(),
            chat_participants: Vec::new(),
        })
    })?;
    collect_rows(rows)
}

// ---------- Tail hydration -----------------------------------------------

/// Walk the candidate list and fill in `conversation_tail` for every
/// email and Teams candidate. Meeting candidates keep an empty tail.
/// We issue one query per candidate (small N: capped at 20), each
/// returning up to `TAIL_CAP` rows; total work is bounded.
fn hydrate_tails(
    conn: &Connection,
    candidates: &mut [WaitingCandidate],
) -> rusqlite::Result<()> {
    for c in candidates.iter_mut() {
        match c.source_kind.as_str() {
            "email" => {
                c.conversation_tail = email_tail(conn, &c.source_ref_id, c.since_ms)?;
            }
            "teams" => {
                c.conversation_tail = teams_tail(conn, &c.source_ref_id, c.since_ms)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// For Teams candidates in group chats (> 2 distinct members), attach
/// the participant list so the resolution LLM can judge whether a
/// message was directed at the user specifically or addressed the
/// group broadly (#125). 1:1 chats and non-Teams candidates leave
/// `chat_participants` empty — `skip_serializing_if` then omits the
/// field from the prompt payload entirely.
fn hydrate_chat_participants(
    conn: &Connection,
    candidates: &mut [WaitingCandidate],
) -> rusqlite::Result<()> {
    for c in candidates.iter_mut() {
        if c.source_kind != "teams" {
            continue;
        }
        let chat_id: Option<String> = conn
            .query_row(
                "SELECT chat_id FROM teams_messages WHERE id = ?1",
                params![&c.source_ref_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(chat_id) = chat_id else { continue };

        let n_members: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT aad_id) FROM teams_chat_members WHERE chat_id = ?1",
                params![&chat_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if n_members <= 2 {
            continue;
        }

        let mut stmt = conn.prepare(
            "SELECT COALESCE(tm.display_name, tcm.display_name, '') AS name, tcm.is_self \
               FROM teams_chat_members tcm \
               LEFT JOIN team_members tm ON tm.id = tcm.team_member_id \
              WHERE tcm.chat_id = ?1 \
              ORDER BY tcm.is_self DESC, name ASC",
        )?;
        let rows = stmt.query_map(params![&chat_id], |r| {
            Ok(ChatParticipant {
                display_name: r.get::<_, String>(0)?,
                is_self: r.get::<_, i64>(1)? != 0,
            })
        })?;
        c.chat_participants = rows.filter_map(Result::ok).collect();
    }
    Ok(())
}

fn email_tail(
    conn: &Connection,
    msg_id: &str,
    after_ms: i64,
) -> rusqlite::Result<Vec<TailEntry>> {
    let sql = "\
        WITH self_emails AS ( \
            SELECT lower(a.value) AS email FROM team_member_aliases a \
              JOIN team_members m ON m.id = a.member_id \
             WHERE a.kind = 'email' AND m.is_self = 1 \
        ), \
        tgt AS ( SELECT thread_id FROM email_messages WHERE id = ?1 ) \
        SELECT em.sent_at_ms, \
               CASE WHEN lower(em.from_email) IN (SELECT email FROM self_emails) \
                    THEN 'self' ELSE 'them' END AS from_kind, \
               COALESCE(NULLIF(em.body_preview, ''), em.subject, '') AS preview \
          FROM email_messages em \
         WHERE em.thread_id = (SELECT thread_id FROM tgt) \
           AND em.sent_at_ms > ?2 \
         ORDER BY em.sent_at_ms ASC \
         LIMIT ?3";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![msg_id, after_ms, TAIL_CAP as i64], |r| {
        Ok(TailEntry {
            ms: r.get(0)?,
            from_kind: r.get(1)?,
            preview: truncate(&r.get::<_, String>(2)?, TAIL_PREVIEW_CHARS),
        })
    })?;
    rows.collect()
}

fn teams_tail(
    conn: &Connection,
    msg_id: &str,
    after_ms: i64,
) -> rusqlite::Result<Vec<TailEntry>> {
    let sql = "\
        WITH self_emails AS ( \
            SELECT lower(a.value) AS email FROM team_member_aliases a \
              JOIN team_members m ON m.id = a.member_id \
             WHERE a.kind = 'email' AND m.is_self = 1 \
        ), \
        tgt AS ( SELECT chat_id FROM teams_messages WHERE id = ?1 ) \
        SELECT tm.sent_at_ms, \
               CASE \
                 WHEN lower(COALESCE(tm.from_email, '')) IN (SELECT email FROM self_emails) THEN 'self' \
                 WHEN EXISTS ( \
                     SELECT 1 FROM teams_chat_members scm \
                      WHERE scm.chat_id = tm.chat_id \
                        AND scm.is_self = 1 \
                        AND tm.from_aad_id IS NOT NULL \
                        AND tm.from_aad_id = scm.aad_id \
                 ) THEN 'self' \
                 ELSE 'them' \
               END AS from_kind, \
               COALESCE(tm.body_preview, '') AS preview \
          FROM teams_messages tm \
         WHERE tm.chat_id = (SELECT chat_id FROM tgt) \
           AND tm.sent_at_ms > ?2 \
         ORDER BY tm.sent_at_ms ASC \
         LIMIT ?3";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![msg_id, after_ms, TAIL_CAP as i64], |r| {
        Ok(TailEntry {
            ms: r.get(0)?,
            from_kind: r.get(1)?,
            preview: truncate(&r.get::<_, String>(2)?, TAIL_PREVIEW_CHARS),
        })
    })?;
    rows.collect()
}

fn truncate(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let cut: String = s.chars().take(cap.saturating_sub(1)).collect();
    format!("{cut}\u{2026}")
}

fn collect_rows<I>(rows: I) -> rusqlite::Result<Vec<WaitingCandidate>>
where
    I: Iterator<Item = rusqlite::Result<WaitingCandidate>>,
{
    let mut out = Vec::new();
    for r in rows {
        let c = r?;
        // Pre-filter obvious social acks / signature-only previews
        // (#120 polish). Spares the LLM from having to reason about
        // them and prevents "Thank you! [signature]" false positives.
        // Meeting candidates are exempt — their preview is built from
        // the title, not the body, and "Meeting: …" is always
        // substantive enough to consider.
        if c.source_kind != "meeting" && !is_substantive_preview(&c.preview) {
            continue;
        }
        out.push(c);
    }
    Ok(out)
}

// ---------- Tests ---------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn open_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::apply_migrations(&conn).unwrap();
        conn
    }

    fn seed_self(conn: &Connection, id: &str, email: &str) {
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, is_self, created_ms, updated_ms) \
             VALUES (?1, 'Me', '', 1, 0, 0)",
            params![id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO team_member_aliases(member_id, kind, value) VALUES (?1, 'email', ?2)",
            params![id, email],
        )
        .unwrap();
    }

    fn seed_teammate(conn: &Connection, id: &str, email: &str) {
        conn.execute(
            "INSERT INTO team_members(id, display_name, role, is_self, created_ms, updated_ms) \
             VALUES (?1, ?1, '', 0, 0, 0)",
            params![id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO team_member_aliases(member_id, kind, value) VALUES (?1, 'email', ?2)",
            params![id, email],
        )
        .unwrap();
    }

    fn seed_connector(conn: &Connection) {
        conn.execute(
            "INSERT OR IGNORE INTO connectors(id, kind, display_name, enabled, config_json, created_ms, updated_ms) \
             VALUES ('mg:test', 'microsoft_graph', 'Test', 1, '{}', 0, 0)",
            [],
        )
        .unwrap();
    }

    fn seed_email(
        conn: &Connection,
        id: &str,
        thread: &str,
        from: &str,
        sent_at: i64,
        preview: &str,
    ) {
        seed_connector(conn);
        conn.execute(
            "INSERT INTO email_messages(id, connector_id, external_id, thread_id, subject, \
                                          from_email, sent_at_ms, body_preview, modified_ms) \
             VALUES (?1, 'mg:test', ?1, ?2, 'Sub', ?3, ?4, ?5, ?4)",
            params![id, thread, from, sent_at, preview],
        )
        .unwrap();
    }

    fn seed_recipient(conn: &Connection, message_id: &str, email: &str, member_id: Option<&str>) {
        conn.execute(
            "INSERT INTO email_recipients(message_id, email, recipient_type, team_member_id) \
             VALUES (?1, ?2, 'to', ?3)",
            params![message_id, email, member_id],
        )
        .unwrap();
    }

    fn seed_teams_chat_member(
        conn: &Connection,
        chat: &str,
        aad: &str,
        email: Option<&str>,
        member_id: Option<&str>,
        is_self: bool,
    ) {
        conn.execute(
            "INSERT INTO teams_chat_members(chat_id, aad_id, email, team_member_id, is_self) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![chat, aad, email, member_id, is_self as i64],
        )
        .unwrap();
    }

    fn seed_teams_msg(
        conn: &Connection,
        id: &str,
        chat: &str,
        from_email: Option<&str>,
        from_aad: Option<&str>,
        sent_at: i64,
        preview: &str,
    ) {
        seed_connector(conn);
        conn.execute(
            "INSERT INTO teams_messages(id, connector_id, external_id, chat_id, chat_kind, \
                                          sent_at_ms, from_aad_id, from_email, body_preview, \
                                          modified_ms) \
             VALUES (?1, 'mg:test', ?1, ?2, 'oneOnOne', ?3, ?4, ?5, ?6, ?3)",
            params![id, chat, sent_at, from_aad, from_email, preview],
        )
        .unwrap();
    }

    fn seed_meeting(
        conn: &Connection,
        id: &str,
        start: i64,
        end: i64,
        linked_note: Option<&str>,
    ) {
        seed_connector(conn);
        conn.execute(
            "INSERT INTO calendar_events(id, connector_id, external_id, title, start_ms, end_ms, \
                                          all_day, modified_ms, linked_note_id) \
             VALUES (?1, 'mg:test', ?1, 'M', ?2, ?3, 0, ?2, ?4)",
            params![id, start, end, linked_note],
        )
        .unwrap();
    }

    fn seed_attendee(
        conn: &Connection,
        event: &str,
        email: &str,
        member_id: Option<&str>,
        is_self: bool,
        is_organizer: bool,
        response: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO calendar_attendees(event_id, email, response_status, is_self, \
                                              is_organizer, team_member_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![event, email, response, is_self as i64, is_organizer as i64, member_id],
        )
        .unwrap();
    }

    // ---------- Substance filter -----------------------------------------

    #[test]
    fn substance_filter_drops_thanks_plus_signature() {
        // The exact false-positive that surfaced for Heike in the wild.
        assert!(!is_substantive_preview(
            "Thank you! Heike Epple Operations Manager | ELAN Languages"
        ));
    }

    #[test]
    fn substance_filter_drops_short_acks() {
        assert!(!is_substantive_preview("Danke fürs Update"));
        assert!(!is_substantive_preview("Thanks!"));
        assert!(!is_substantive_preview("OK"));
        assert!(!is_substantive_preview("Perfect, danke"));
        assert!(!is_substantive_preview("got it"));
    }

    #[test]
    fn substance_filter_keeps_questions_even_after_ack() {
        // "Danke" front, but a real question follows.
        assert!(is_substantive_preview(
            "Danke für dein Update. Wer kommt heute zum Meeting?"
        ));
        assert!(is_substantive_preview("Können wir da unterstützen?"));
        assert!(is_substantive_preview("any update on the rollout?"));
    }

    #[test]
    fn substance_filter_keeps_substantive_no_question() {
        assert!(is_substantive_preview(
            "Kannst du dir bitte die folgenden Daten aus dem PK App exportieren"
        ));
        assert!(is_substantive_preview(
            "Hey, was ist denn aus den bridge Zugängen geworden"
        ));
    }

    #[test]
    fn signals_query_skips_thanks_plus_signature_candidate() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_email(
            &conn, "e_thanks", "t1", "alice@x.io",
            now - 1_000,
            "Thank you! Alice Example, Engineering Manager | Example Inc.",
        );
        seed_recipient(&conn, "e_thanks", "me@x.io", Some("tm_self"));

        let got = inbound_email(&conn, "tm_alice", now - RECENCY_WINDOW_MS).unwrap();
        assert!(got.is_empty(), "Thank-you-with-signature must be filtered before reaching the LLM");
    }

    // ---------- Email ------------------------------------------------------

    #[test]
    fn inbound_email_picks_unanswered() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_email(&conn, "e1", "t1", "alice@x.io", now - 1_000, "ping?");
        seed_recipient(&conn, "e1", "me@x.io", Some("tm_self"));

        let got = inbound_email(&conn, "tm_alice", now - RECENCY_WINDOW_MS).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].source_ref_id, "e1");
        assert_eq!(got[0].source_kind, "email");
        assert_eq!(got[0].preview, "ping?");
    }

    #[test]
    fn inbound_email_surfaces_even_with_self_reply() {
        // Resolution is now an LLM judgment: candidates always surface
        // regardless of whether self has replied. The LLM reads the
        // hydrated `conversation_tail` and decides resolved vs pending.
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_email(&conn, "e1", "t1", "alice@x.io", now - 2_000, "any update on the rollout?");
        seed_recipient(&conn, "e1", "me@x.io", Some("tm_self"));
        seed_email(&conn, "e2", "t1", "me@x.io", now - 1_000, "yes shipping today");

        let got = candidates_from_me(&conn, "tm_alice", now).unwrap();
        assert!(got.iter().any(|c| c.source_ref_id == "e1"));
        let cand = got.iter().find(|c| c.source_ref_id == "e1").unwrap();
        assert_eq!(cand.conversation_tail.len(), 1);
        assert_eq!(cand.conversation_tail[0].from_kind, "self");
    }

    #[test]
    fn inbound_email_filters_recency_window() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        // 60 days old → outside the 30-day window.
        let old = now - 60 * 24 * 3_600 * 1_000;
        seed_email(&conn, "e_old", "t1", "alice@x.io", old, "old");
        seed_recipient(&conn, "e_old", "me@x.io", Some("tm_self"));

        let got = inbound_email(&conn, "tm_alice", now - RECENCY_WINDOW_MS).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn inbound_email_recipient_via_alias_match() {
        // Recipient row has no team_member_id; alias-email match path
        // still surfaces it.
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_email(
            &conn, "e1", "t1", "alice@x.io",
            now - 1_000,
            "can you take a look at the Q3 budget rollover?",
        );
        seed_recipient(&conn, "e1", "me@x.io", None);

        let got = inbound_email(&conn, "tm_alice", now - RECENCY_WINDOW_MS).unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn outbound_email_picks_unanswered() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_email(&conn, "e1", "t1", "me@x.io", now - 1_000, "any update?");
        seed_recipient(&conn, "e1", "alice@x.io", Some("tm_alice"));

        let got = outbound_email(&conn, "tm_alice", now - RECENCY_WINDOW_MS).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].source_ref_id, "e1");
    }

    #[test]
    fn outbound_email_surfaces_even_with_their_reply() {
        // Same: candidate surfaces, tail tells the LLM about the reply.
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_email(&conn, "e1", "t1", "me@x.io", now - 2_000, "any update on the rollout?");
        seed_recipient(&conn, "e1", "alice@x.io", Some("tm_alice"));
        seed_email(&conn, "e2", "t1", "alice@x.io", now - 1_000, "yes shipping today");

        let got = candidates_for_them(&conn, "tm_alice", now).unwrap();
        assert!(got.iter().any(|c| c.source_ref_id == "e1"));
        let cand = got.iter().find(|c| c.source_ref_id == "e1").unwrap();
        assert_eq!(cand.conversation_tail.len(), 1);
        assert_eq!(cand.conversation_tail[0].from_kind, "them");
    }

    // ---------- Teams ------------------------------------------------------

    #[test]
    fn inbound_teams_picks_unanswered_via_aad() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_teams_chat_member(&conn, "c1", "aad-self", Some("me@x.io"), Some("tm_self"), true);
        seed_teams_chat_member(
            &conn, "c1", "aad-alice", Some("alice@x.io"), Some("tm_alice"), false,
        );
        seed_teams_msg(&conn, "m1", "c1", None, Some("aad-alice"), now - 1_000, "got a sec?");

        let got = inbound_teams(&conn, "tm_alice", now - RECENCY_WINDOW_MS).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].source_ref_id, "m1");
        assert_eq!(got[0].source_kind, "teams");
    }

    #[test]
    fn inbound_teams_surfaces_with_tail_when_self_replied() {
        // Same as the email version: candidate surfaces, tail tells
        // the LLM the chat history.
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_teams_chat_member(&conn, "c1", "aad-self", Some("me@x.io"), Some("tm_self"), true);
        seed_teams_chat_member(
            &conn, "c1", "aad-alice", Some("alice@x.io"), Some("tm_alice"), false,
        );
        seed_teams_msg(
            &conn, "m1", "c1", None, Some("aad-alice"),
            now - 2_000, "Hey, can you send the file?",
        );
        seed_teams_msg(
            &conn, "m2", "c1", Some("me@x.io"), None,
            now - 1_000, "ok will send tomorrow",
        );

        let got = candidates_from_me(&conn, "tm_alice", now).unwrap();
        assert!(got.iter().any(|c| c.source_ref_id == "m1"));
        let cand = got.iter().find(|c| c.source_ref_id == "m1").unwrap();
        assert_eq!(cand.conversation_tail.len(), 1);
        assert_eq!(cand.conversation_tail[0].from_kind, "self");
        assert!(cand.conversation_tail[0].preview.contains("send tomorrow"));
    }

    #[test]
    fn outbound_teams_picks_unanswered() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_teams_chat_member(&conn, "c1", "aad-self", Some("me@x.io"), Some("tm_self"), true);
        seed_teams_chat_member(
            &conn, "c1", "aad-alice", Some("alice@x.io"), Some("tm_alice"), false,
        );
        seed_teams_msg(
            &conn, "m1", "c1", Some("me@x.io"), None, now - 1_000,
            "ping — any update on the rollout?",
        );

        let got = outbound_teams(&conn, "tm_alice", now - RECENCY_WINDOW_MS).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].source_ref_id, "m1");
    }

    // ---------- Meetings --------------------------------------------------

    #[test]
    fn meeting_past_without_note_picks_up() {
        let conn = open_db();
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_meeting(&conn, "m_past", now - 3_600_000, now - 1_800_000, None);
        seed_attendee(&conn, "m_past", "alice@x.io", Some("tm_alice"), false, false, None);
        seed_meeting(
            &conn, "m_past_with_note", now - 7_200_000, now - 5_400_000,
            Some("note1"),
        );
        seed_attendee(
            &conn, "m_past_with_note", "alice@x.io", Some("tm_alice"), false, false, None,
        );

        let got = meeting_past_without_note(
            &conn, "tm_alice", now - RECENCY_WINDOW_MS, now, MEETING_SUBCAP,
        )
        .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].source_ref_id, "m_past");
        assert_eq!(got[0].source_kind, "meeting");
    }

    #[test]
    fn meeting_future_unaccepted_picks_up() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_meeting(&conn, "m_fut", now + 3_600_000, now + 7_200_000, None);
        // Alice organizes, self attendee row response is needsAction.
        seed_attendee(
            &conn, "m_fut", "alice@x.io", Some("tm_alice"), false, true, None,
        );
        seed_attendee(
            &conn, "m_fut", "me@x.io", Some("tm_self"), true, false, Some("needsAction"),
        );

        let got = meeting_future_unaccepted(&conn, "tm_alice", now, MEETING_SUBCAP).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].source_ref_id, "m_fut");
    }

    #[test]
    fn meeting_future_skips_already_accepted() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        seed_meeting(&conn, "m_fut", now + 3_600_000, now + 7_200_000, None);
        seed_attendee(
            &conn, "m_fut", "alice@x.io", Some("tm_alice"), false, true, None,
        );
        seed_attendee(
            &conn, "m_fut", "me@x.io", Some("tm_self"), true, false, Some("accepted"),
        );

        let got = meeting_future_unaccepted(&conn, "tm_alice", now, MEETING_SUBCAP).unwrap();
        assert!(got.is_empty());
    }

    // ---------- Direction wrappers ----------------------------------------

    #[test]
    fn direction_cap_truncates() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        // 25 unanswered inbound emails — only CANDIDATES_PER_DIRECTION_CAP
        // should survive.
        for i in 0..25 {
            let id = format!("e{i}");
            let thread = format!("t{i}");
            seed_email(
                &conn, &id, &thread, "alice@x.io",
                now - 1_000 - i as i64,
                "Hey can you take a look at this please?",
            );
            seed_recipient(&conn, &id, "me@x.io", Some("tm_self"));
        }

        let got = candidates_from_me(&conn, "tm_alice", now).unwrap();
        assert_eq!(got.len(), CANDIDATES_PER_DIRECTION_CAP);
    }

    #[test]
    fn from_me_combines_email_and_teams_and_meeting() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;

        // 1× inbound email
        seed_email(&conn, "e1", "t1", "alice@x.io", now - 1_000, "any update on the rollout?");
        seed_recipient(&conn, "e1", "me@x.io", Some("tm_self"));
        // 1× inbound teams
        seed_teams_chat_member(&conn, "c1", "aad-self", Some("me@x.io"), Some("tm_self"), true);
        seed_teams_chat_member(
            &conn, "c1", "aad-alice", Some("alice@x.io"), Some("tm_alice"), false,
        );
        seed_teams_msg(&conn, "m1", "c1", None, Some("aad-alice"), now - 2_000, "?");
        // 1× past meeting without note
        seed_meeting(&conn, "mp", now - 5_000, now - 4_000, None);
        seed_attendee(&conn, "mp", "alice@x.io", Some("tm_alice"), false, false, None);

        let got = candidates_from_me(&conn, "tm_alice", now).unwrap();
        let kinds: Vec<&str> = got.iter().map(|c| c.source_kind.as_str()).collect();
        assert!(kinds.contains(&"email"));
        assert!(kinds.contains(&"teams"));
        assert!(kinds.contains(&"meeting"));
        // Recency-desc ordering: email (1k ago) before teams (2k ago) before meeting (5k ago).
        assert_eq!(got[0].source_kind, "email");
    }

    /// Direct INSERT for tests that need to control `chat_kind`
    /// (the shared `seed_teams_msg` hardcodes 'oneOnOne').
    fn seed_teams_msg_with_kind(
        conn: &Connection,
        id: &str,
        chat: &str,
        chat_kind: &str,
        sent_at: i64,
        preview: &str,
    ) {
        seed_connector(conn);
        conn.execute(
            "INSERT INTO teams_messages(id, connector_id, external_id, chat_id, chat_kind, \
                                          sent_at_ms, from_aad_id, body_preview, modified_ms) \
             VALUES (?1, 'mg:test', ?1, ?2, ?3, ?4, 'aad-alice', ?5, ?4)",
            params![id, chat, chat_kind, sent_at, preview],
        )
        .unwrap();
    }

    /// Group chat (4 members) → candidate is hydrated with all
    /// participants, self listed first.
    #[test]
    fn hydrate_chat_participants_includes_group_members() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        seed_teammate(&conn, "tm_bob", "bob@x.io");
        seed_teammate(&conn, "tm_carol", "carol@x.io");
        seed_teams_chat_member(&conn, "c-grp", "aad-self", Some("me@x.io"), Some("tm_self"), true);
        seed_teams_chat_member(&conn, "c-grp", "aad-alice", Some("alice@x.io"), Some("tm_alice"), false);
        seed_teams_chat_member(&conn, "c-grp", "aad-bob", Some("bob@x.io"), Some("tm_bob"), false);
        seed_teams_chat_member(&conn, "c-grp", "aad-carol", Some("carol@x.io"), Some("tm_carol"), false);
        seed_teams_msg_with_kind(&conn, "m-grp", "c-grp", "group", 1_000, "Hey team, wer kann das übernehmen?");

        let mut cands = vec![WaitingCandidate {
            source_kind: "teams".into(),
            source_ref_id: "m-grp".into(),
            since_ms: 1_000,
            preview: "Hey team, wer kann das übernehmen?".into(),
            conversation_tail: Vec::new(),
            chat_participants: Vec::new(),
        }];
        hydrate_chat_participants(&conn, &mut cands).unwrap();

        assert_eq!(cands[0].chat_participants.len(), 4);
        assert!(cands[0].chat_participants[0].is_self,
            "self must sort to the front so the LLM scans it first");
        let names: Vec<&str> = cands[0].chat_participants[1..]
            .iter()
            .map(|p| p.display_name.as_str())
            .collect();
        // Non-self entries are alphabetical by display_name.
        assert_eq!(names, vec!["tm_alice", "tm_bob", "tm_carol"]);
    }

    /// 1:1 chat (2 members) → hydration is a no-op so the prompt
    /// payload stays compact and `skip_serializing_if` drops the key.
    #[test]
    fn hydrate_chat_participants_skips_one_on_one_chats() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        seed_teams_chat_member(&conn, "c-1on1", "aad-self", Some("me@x.io"), Some("tm_self"), true);
        seed_teams_chat_member(&conn, "c-1on1", "aad-alice", Some("alice@x.io"), Some("tm_alice"), false);
        seed_teams_msg_with_kind(&conn, "m-d1", "c-1on1", "oneOnOne", 1_000, "Hi!");

        let mut cands = vec![WaitingCandidate {
            source_kind: "teams".into(),
            source_ref_id: "m-d1".into(),
            since_ms: 1_000,
            preview: "Hi!".into(),
            conversation_tail: Vec::new(),
            chat_participants: Vec::new(),
        }];
        hydrate_chat_participants(&conn, &mut cands).unwrap();
        assert!(cands[0].chat_participants.is_empty());
    }

    /// Email and meeting candidates already carry recipient context
    /// elsewhere; the hydration path must skip them entirely so we
    /// don't pollute prompts with irrelevant Teams data.
    #[test]
    fn hydrate_chat_participants_is_noop_for_email_and_meeting_candidates() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        // Existence of a Teams group chat in the DB shouldn't matter —
        // these candidates aren't Teams.
        seed_teams_chat_member(&conn, "c-grp", "aad-self", Some("me@x.io"), Some("tm_self"), true);
        seed_teams_chat_member(&conn, "c-grp", "aad-alice", Some("alice@x.io"), Some("tm_alice"), false);
        seed_teams_chat_member(&conn, "c-grp", "aad-bob", Some("bob@x.io"), None, false);

        let mut cands = vec![
            WaitingCandidate {
                source_kind: "email".into(),
                source_ref_id: "e1".into(),
                since_ms: 1_000,
                preview: "?".into(),
                conversation_tail: Vec::new(),
                chat_participants: Vec::new(),
            },
            WaitingCandidate {
                source_kind: "meeting".into(),
                source_ref_id: "mp".into(),
                since_ms: 1_000,
                preview: "M".into(),
                conversation_tail: Vec::new(),
                chat_participants: Vec::new(),
            },
        ];
        hydrate_chat_participants(&conn, &mut cands).unwrap();
        assert!(cands[0].chat_participants.is_empty());
        assert!(cands[1].chat_participants.is_empty());
    }

    // ---------- Recently rejected waiting (#149) --------------------------

    #[allow(clippy::too_many_arguments)]
    fn seed_deletion(
        conn: &Connection,
        deleted_ms: i64,
        origin_synth_kind: &str,
        text: &str,
        subject_member_id: Option<&str>,
        assignee_id: Option<&str>,
        cause: &str,
    ) {
        conn.execute(
            "INSERT INTO action_deletions \
                (deleted_ms, origin_kind, origin_synth_kind, \
                 subject_member_id, assignee_id, text, cause) \
             VALUES (?1, 'synth', ?2, ?3, ?4, ?5, ?6)",
            params![
                deleted_ms,
                origin_synth_kind,
                subject_member_id,
                assignee_id,
                text,
                cause
            ],
        )
        .unwrap();
    }

    #[test]
    fn worker_rejected_block_groups_by_synth_kind() {
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now = 1_700_000_000_000;
        // One row of each kind for the same counterparty.
        seed_deletion(
            &conn,
            now - 1_000,
            "email_waiting",
            "Reply to Alice about Q3",
            Some("tm_alice"),
            Some("tm_self"),
            "user_delete",
        );
        seed_deletion(
            &conn,
            now - 2_000,
            "teams_waiting",
            "Follow up on planning ping",
            Some("tm_alice"),
            Some("tm_self"),
            "user_dismiss",
        );
        seed_deletion(
            &conn,
            now - 3_000,
            "meeting_waiting",
            "Write notes from 1:1",
            Some("tm_self"),
            Some("tm_alice"),
            "user_delete",
        );

        let payload = recently_rejected_waiting(&conn, "tm_alice", now)
            .unwrap()
            .expect("expected payload with all three kinds populated");
        let obj = payload.as_object().unwrap();
        assert_eq!(obj.len(), 3);
        assert!(obj.contains_key("email"));
        assert!(obj.contains_key("teams"));
        assert!(obj.contains_key("meeting"));
        let emails = obj["email"].as_array().unwrap();
        assert_eq!(emails.len(), 1);
        assert_eq!(emails[0].as_str().unwrap(), "Reply to Alice about Q3");
    }

    #[test]
    fn worker_rejected_block_filters_to_window() {
        let conn = open_db();
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        // `now` chosen large enough that `now - 90d` is positive — keeps
        // the seeded `deleted_ms` non-negative, which matches production
        // (ms-since-epoch is always positive).
        let now: i64 = 100 * 24 * 60 * 60 * 1000; // 100 days
        seed_deletion(
            &conn,
            now - 40 * 24 * 60 * 60 * 1000, // 40 days ago — outside 30-day window
            "email_waiting",
            "Stale rejection",
            Some("tm_alice"),
            None,
            "user_delete",
        );
        seed_deletion(
            &conn,
            now - 5 * 24 * 60 * 60 * 1000, // 5 days ago — inside window
            "email_waiting",
            "Recent rejection",
            Some("tm_alice"),
            None,
            "user_delete",
        );
        let payload = recently_rejected_waiting(&conn, "tm_alice", now).unwrap().unwrap();
        let emails = payload["email"].as_array().unwrap();
        assert_eq!(emails.len(), 1, "only the in-window row survives");
        assert_eq!(emails[0].as_str().unwrap(), "Recent rejection");
    }

    #[test]
    fn worker_rejected_block_excludes_auto_resolved() {
        let conn = open_db();
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now: i64 = 100 * 24 * 60 * 60 * 1000;
        seed_deletion(
            &conn,
            now - 1_000,
            "email_waiting",
            "Worker swept this",
            Some("tm_alice"),
            None,
            "auto_resolved",
        );
        let payload = recently_rejected_waiting(&conn, "tm_alice", now).unwrap();
        assert!(payload.is_none(), "auto_resolved must not feed the prompt");
    }

    #[test]
    fn worker_rejected_block_matches_by_assignee_for_for_them_rows() {
        // `waiting_for_them` rows have assignee = person, subject = self.
        // The query must catch those via the assignee_id arm.
        let conn = open_db();
        seed_self(&conn, "tm_self", "me@x.io");
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now: i64 = 100 * 24 * 60 * 60 * 1000;
        seed_deletion(
            &conn,
            now - 1_000,
            "teams_waiting",
            "Ping Alice for status",
            Some("tm_self"),     // subject = self (it's a "for_them" row)
            Some("tm_alice"),    // assignee = person
            "user_delete",
        );
        let payload = recently_rejected_waiting(&conn, "tm_alice", now)
            .unwrap()
            .expect("for_them row must be visible when person is the assignee");
        let teams = payload["teams"].as_array().unwrap();
        assert_eq!(teams.len(), 1);
    }

    #[test]
    fn worker_rejected_block_caps_per_kind() {
        let conn = open_db();
        seed_teammate(&conn, "tm_alice", "alice@x.io");
        let now: i64 = 100 * 24 * 60 * 60 * 1000;
        // Seed twice the cap; only the most recent REJECTED_WAITING_PER_KIND_CAP
        // survive.
        for i in 0..(REJECTED_WAITING_PER_KIND_CAP * 2) {
            seed_deletion(
                &conn,
                now - (i as i64) * 1_000,
                "email_waiting",
                &format!("Item #{i:02}"),
                Some("tm_alice"),
                None,
                "user_delete",
            );
        }
        let payload = recently_rejected_waiting(&conn, "tm_alice", now).unwrap().unwrap();
        let emails = payload["email"].as_array().unwrap();
        assert_eq!(emails.len(), REJECTED_WAITING_PER_KIND_CAP);
        // Newest survives, oldest dropped.
        assert!(emails.iter().any(|v| v.as_str().unwrap().contains("#00")));
        assert!(
            !emails.iter().any(|v| v.as_str().unwrap().contains(&format!(
                "#{:02}",
                REJECTED_WAITING_PER_KIND_CAP * 2 - 1
            ))),
            "oldest row must be dropped past the cap"
        );
    }
}
