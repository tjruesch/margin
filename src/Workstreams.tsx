//! Workstreams view (#71).
//!
//! Sidebar nav target. List of synthesized workstreams as cards;
//! click → detail view with sections for actions, emails, meetings,
//! notes. Refresh button forces a synthesis pass via the boot
//! pipeline added in #70 and listens for `workstream-status` to
//! refetch.

import { useCallback, useEffect, useState } from "react";

import {
  type EmailMessage,
  type Workstream,
  type WorkstreamAction,
  type WorkstreamDetail,
  type WorkstreamStatus,
  getEmailBody,
  getWorkstreamDetails,
  openOrCreateEventNote,
  setWorkstreamActionDone,
  setWorkstreamStatus,
} from "./file";
import { IconChevLeft } from "./icons";

// ----- List view -----------------------------------------------------------

export function WorkstreamsView({
  workstreams,
  loading,
  synthInFlight,
  synthMessage,
  onRefresh,
  onOpenNote,
}: {
  workstreams: Workstream[];
  loading: boolean;
  synthInFlight: boolean;
  synthMessage: string | null;
  onRefresh: () => void;
  onOpenNote: (path: string) => void;
}) {
  const [selectedId, setSelectedId] = useState<string | null>(null);

  if (selectedId) {
    return (
      <WorkstreamDetailView
        id={selectedId}
        onBack={() => setSelectedId(null)}
        onOpenNote={onOpenNote}
      />
    );
  }

  const nowMs = Date.now();

  return (
    <div className="workstream-view">
      <header className="workstream-header">
        <div>
          <h1 className="workstream-title">Workstreams</h1>
          <p className="workstream-subtitle">
            Ongoing efforts synthesized from emails, meetings, and notes.
          </p>
        </div>
        <button
          type="button"
          className="workstream-refresh"
          onClick={onRefresh}
          disabled={synthInFlight}
        >
          {synthInFlight ? (
            <>
              <span className="workstream-spinner" aria-hidden />
              Synthesizing…
            </>
          ) : (
            "Refresh"
          )}
        </button>
      </header>

      {synthMessage ? (
        <div className="workstream-toast" role="status">
          {synthMessage}
        </div>
      ) : null}

      {loading ? (
        <p className="home-empty">Loading…</p>
      ) : workstreams.length === 0 ? (
        <div className="workstream-empty">
          <p>No workstreams yet.</p>
          <p>
            Connect Microsoft in Settings to ingest your inbox and calendar,
            then click Refresh to synthesize.
          </p>
        </div>
      ) : (
        <div className="workstream-list">
          {workstreams.map((w) => (
            <button
              type="button"
              key={w.id}
              className="workstream-card"
              onClick={() => setSelectedId(w.id)}
            >
              <div className="workstream-card-head">
                <span className="workstream-card-title">{w.title}</span>
                <span className="workstream-card-time">
                  {formatPast(w.last_activity_ms, nowMs)}
                </span>
              </div>
              <p className="workstream-card-summary">{w.summary}</p>
              <div className="workstream-card-counts">
                {countLine(w)}
              </div>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

function countLine(w: Workstream): string {
  const parts: string[] = [];
  if (w.open_action_count > 0)
    parts.push(plural(w.open_action_count, "open action", "open actions"));
  if (w.email_count > 0) parts.push(plural(w.email_count, "email", "emails"));
  if (w.event_count > 0)
    parts.push(plural(w.event_count, "meeting", "meetings"));
  if (w.note_count > 0) parts.push(plural(w.note_count, "note", "notes"));
  return parts.length ? parts.join(" · ") : "No items yet";
}

function plural(n: number, singular: string, plural_: string): string {
  return `${n} ${n === 1 ? singular : plural_}`;
}

// ----- Detail view ---------------------------------------------------------

function WorkstreamDetailView({
  id,
  onBack,
  onOpenNote,
}: {
  id: string;
  onBack: () => void;
  onOpenNote: (path: string) => void;
}) {
  const [detail, setDetail] = useState<WorkstreamDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [missing, setMissing] = useState(false);

  const reload = useCallback(async () => {
    setLoading(true);
    try {
      const d = await getWorkstreamDetails(id);
      if (!d) {
        setMissing(true);
        setDetail(null);
      } else {
        setMissing(false);
        setDetail(d);
      }
    } catch (e) {
      console.error("[workstreams] detail fetch failed", e);
      setMissing(true);
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    void reload();
  }, [reload]);

  // Optimistic update for action toggle. On error, revert and refetch
  // to reconcile.
  const onToggleAction = useCallback(
    async (actionId: string, nextDone: boolean) => {
      setDetail((d) => {
        if (!d) return d;
        return {
          ...d,
          actions: d.actions.map((a) =>
            a.id === actionId ? { ...a, done: nextDone } : a,
          ),
        };
      });
      try {
        await setWorkstreamActionDone(actionId, nextDone);
      } catch (e) {
        console.error("[workstreams] toggle action failed", e);
        await reload();
      }
    },
    [reload],
  );

  const onChangeStatus = useCallback(
    async (status: WorkstreamStatus) => {
      try {
        await setWorkstreamStatus(id, status);
        if (status !== "active") {
          // Archived/snoozed workstreams drop off the list, so go back.
          onBack();
        } else {
          await reload();
        }
      } catch (e) {
        console.error("[workstreams] set status failed", e);
        await reload();
      }
    },
    [id, onBack, reload],
  );

  const onOpenEvent = useCallback(
    async (eventId: string) => {
      try {
        const path = await openOrCreateEventNote(eventId);
        onOpenNote(path);
      } catch (e) {
        console.error("[workstreams] open event note failed", e);
      }
    },
    [onOpenNote],
  );

  if (loading && !detail) {
    return (
      <div className="workstream-view">
        <DetailHeader title="" onBack={onBack} status={null} onChangeStatus={() => {}} />
        <p className="home-empty">Loading…</p>
      </div>
    );
  }
  if (missing || !detail) {
    return (
      <div className="workstream-view">
        <DetailHeader title="Workstream" onBack={onBack} status={null} onChangeStatus={() => {}} />
        <p className="home-empty">Workstream not found.</p>
      </div>
    );
  }

  return (
    <div className="workstream-view">
      <DetailHeader
        title={detail.title}
        onBack={onBack}
        status={detail.status}
        onChangeStatus={onChangeStatus}
      />
      <p className="workstream-detail-summary">{detail.summary}</p>

      <ActionsSection actions={detail.actions} onToggle={onToggleAction} />

      <EmailsSection emails={detail.emails} />

      <MeetingsSection
        events={detail.events}
        onOpenEvent={onOpenEvent}
      />

      <NotesSection notes={detail.notes} onOpenNote={onOpenNote} />
    </div>
  );
}

function DetailHeader({
  title,
  onBack,
  status,
  onChangeStatus,
}: {
  title: string;
  onBack: () => void;
  status: WorkstreamStatus | null;
  onChangeStatus: (s: WorkstreamStatus) => void | Promise<void>;
}) {
  return (
    <header className="workstream-header workstream-detail-header">
      <button
        type="button"
        className="workstream-back"
        onClick={onBack}
        aria-label="Back to workstreams"
      >
        <IconChevLeft size={20} />
        Back
      </button>
      <h1 className="workstream-title">{title}</h1>
      {status ? (
        <select
          className="workstream-status"
          value={status}
          onChange={(e) => onChangeStatus(e.target.value as WorkstreamStatus)}
          aria-label="Workstream status"
        >
          <option value="active">Active</option>
          <option value="snoozed">Snoozed</option>
          <option value="archived">Archived</option>
        </select>
      ) : null}
    </header>
  );
}

// ----- Sections ------------------------------------------------------------

function ActionsSection({
  actions,
  onToggle,
}: {
  actions: WorkstreamAction[];
  onToggle: (actionId: string, nextDone: boolean) => void | Promise<void>;
}) {
  if (actions.length === 0) return null;
  return (
    <section className="workstream-section">
      <h2 className="workstream-section-title">Actions ({actions.length})</h2>
      <ul className="workstream-actions">
        {actions.map((a) => (
          <li
            key={a.id}
            className={`workstream-action-row ${a.done ? "is-done" : ""}`}
          >
            <label className="workstream-action-check">
              <input
                type="checkbox"
                checked={a.done}
                onChange={(e) => onToggle(a.id, e.target.checked)}
              />
              <span>{a.text}</span>
            </label>
            <span className="workstream-action-source">
              from {a.source_kind}
            </span>
          </li>
        ))}
      </ul>
    </section>
  );
}

function EmailsSection({ emails }: { emails: EmailMessage[] }) {
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [bodies, setBodies] = useState<Record<string, BodyState>>({});

  if (emails.length === 0) return null;

  const toggle = async (m: EmailMessage) => {
    const isOpen = !!expanded[m.id];
    setExpanded({ ...expanded, [m.id]: !isOpen });
    if (isOpen) return;
    if (bodies[m.id]) return; // already loaded or loading

    if (m.body_html) {
      setBodies((b) => ({ ...b, [m.id]: { kind: "loaded", html: m.body_html! } }));
      return;
    }
    setBodies((b) => ({ ...b, [m.id]: { kind: "loading" } }));
    try {
      const html = await getEmailBody(m.id);
      if (html) {
        setBodies((b) => ({ ...b, [m.id]: { kind: "loaded", html } }));
      } else {
        setBodies((b) => ({
          ...b,
          [m.id]: { kind: "empty" },
        }));
      }
    } catch (e) {
      console.error("[workstreams] email body fetch failed", e);
      setBodies((b) => ({
        ...b,
        [m.id]: { kind: "error" },
      }));
    }
  };

  return (
    <section className="workstream-section">
      <h2 className="workstream-section-title">Emails ({emails.length})</h2>
      <ul className="workstream-emails">
        {emails.map((m) => {
          const open = !!expanded[m.id];
          const body = bodies[m.id];
          const date = formatShortDate(m.sent_at_ms);
          const fromLabel = m.from_name || m.from_email;
          return (
            <li key={m.id} className="workstream-email">
              <button
                type="button"
                className="workstream-email-row"
                onClick={() => void toggle(m)}
                aria-expanded={open}
              >
                <span className="workstream-email-date">{date}</span>
                <span className="workstream-email-from">{fromLabel}</span>
                <span className="workstream-email-subject">{m.subject}</span>
                <span className="workstream-email-chev">{open ? "▾" : "▸"}</span>
              </button>
              {open ? (
                <div className="workstream-email-body">
                  <EmailBodyPanel body={body} fallbackPreview={m.body_preview} />
                </div>
              ) : null}
            </li>
          );
        })}
      </ul>
    </section>
  );
}

type BodyState =
  | { kind: "loading" }
  | { kind: "loaded"; html: string }
  | { kind: "empty" }
  | { kind: "error" };

function EmailBodyPanel({
  body,
  fallbackPreview,
}: {
  body: BodyState | undefined;
  fallbackPreview: string | null;
}) {
  if (!body || body.kind === "loading") {
    return <p className="workstream-email-loading">Loading…</p>;
  }
  if (body.kind === "loaded") {
    // Render in a sandboxed iframe so any leftover script in the
    // email's HTML can't reach Margin's main DOM. `allow-scripts` is
    // deliberately omitted; outbound links are dead-ends in v1.
    return (
      <iframe
        title="Email body"
        className="workstream-email-iframe"
        sandbox="allow-same-origin"
        srcDoc={body.html}
      />
    );
  }
  if (body.kind === "error") {
    return (
      <p className="workstream-email-loading">
        Couldn't load email body — it may have been deleted.
        {fallbackPreview ? (
          <>
            <br />
            <span>Preview: {fallbackPreview}</span>
          </>
        ) : null}
      </p>
    );
  }
  // empty
  return (
    <p className="workstream-email-loading">
      {fallbackPreview ?? "(no body)"}
    </p>
  );
}

function MeetingsSection({
  events,
  onOpenEvent,
}: {
  events: WorkstreamDetail["events"];
  onOpenEvent: (eventId: string) => void | Promise<void>;
}) {
  if (events.length === 0) return null;
  return (
    <section className="workstream-section">
      <h2 className="workstream-section-title">Meetings ({events.length})</h2>
      <ul className="workstream-meetings">
        {events.map((e) => (
          <li key={e.id}>
            <button
              type="button"
              className="workstream-meeting-row"
              onClick={() => void onOpenEvent(e.id)}
            >
              <span className="workstream-meeting-date">
                {formatShortDateTime(e.start_ms)}
              </span>
              <span className="workstream-meeting-title">{e.title}</span>
              {e.attendees.length > 0 ? (
                <span className="workstream-meeting-attendees">
                  {e.attendees
                    .slice(0, 4)
                    .map((a) => a.display_name || a.email)
                    .join(", ")}
                  {e.attendees.length > 4
                    ? ` +${e.attendees.length - 4}`
                    : ""}
                </span>
              ) : null}
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}

function NotesSection({
  notes,
  onOpenNote,
}: {
  notes: WorkstreamDetail["notes"];
  onOpenNote: (path: string) => void;
}) {
  if (notes.length === 0) return null;
  return (
    <section className="workstream-section">
      <h2 className="workstream-section-title">Notes ({notes.length})</h2>
      <ul className="workstream-notes">
        {notes.map((n) => (
          <li key={n.note_path}>
            <button
              type="button"
              className="workstream-note-row"
              onClick={() => onOpenNote(n.note_path)}
            >
              <span className="workstream-note-date">
                {formatShortDate(n.modified_ms)}
              </span>
              <span className="workstream-note-title">
                {n.title || n.note_path}
              </span>
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}

// ----- Time helpers --------------------------------------------------------

function formatPast(ms: number, nowMs: number): string {
  const delta = nowMs - ms;
  if (delta < 60_000) return "just now";
  const min = Math.floor(delta / 60_000);
  if (min < 60) return `${min}m ago`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr}h ago`;
  const days = Math.floor(hr / 24);
  if (days === 1) return "yesterday";
  if (days < 7) return `${days}d ago`;
  return formatShortDate(ms);
}

function formatShortDate(ms: number): string {
  if (!ms) return "";
  return new Intl.DateTimeFormat([], {
    month: "short",
    day: "numeric",
  }).format(new Date(ms));
}

function formatShortDateTime(ms: number): string {
  if (!ms) return "";
  return new Intl.DateTimeFormat([], {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  }).format(new Date(ms));
}

