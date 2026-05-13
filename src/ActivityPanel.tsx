import { useEffect, useRef, useState } from "react";

import {
  getDailyActivity,
  listRecentActivity,
  type ActivityEventRow,
  type DailyActivitySummary,
} from "./file";

type Props = {
  open: boolean;
  onClose: () => void;
  /** Activated when the user clicks a row in the RECENT section (#116).
   *  Implementations should switch nav to Team detail for `memberId`
   *  and seed `highlightObsId` so the SuggestionsTab scrolls + flashes
   *  the matching accepted row. `highlightObsId` is null for the
   *  profile_snapshot_created kind, which lands on the Profile tab. */
  onOpenObservation: (memberId: string, highlightObsId: string | null) => void;
};

export function ActivityPanel({ open, onClose, onOpenObservation }: Props) {
  const panelRef = useRef<HTMLDivElement | null>(null);
  const [summary, setSummary] = useState<DailyActivitySummary | null>(null);
  const [events, setEvents] = useState<ActivityEventRow[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Outside-click + Escape dismissal.
  useEffect(() => {
    if (!open) return;
    const onMouseDown = (e: MouseEvent) => {
      const target = e.target as Node | null;
      if (panelRef.current && target && panelRef.current.contains(target)) return;
      onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("mousedown", onMouseDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onMouseDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [open, onClose]);

  // Refetch every time the popover opens. No caching.
  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    void (async () => {
      try {
        const [s, ev] = await Promise.all([
          getDailyActivity(),
          listRecentActivity(),
        ]);
        if (cancelled) return;
        setSummary(s);
        setEvents(ev);
      } catch (e) {
        if (!cancelled) setError(String(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [open]);

  if (!open) return null;

  const rows = summary ? buildRows(summary) : [];
  const isEmpty =
    !loading && !error && summary != null && rows.length === 0 && events.length === 0;

  return (
    <div
      ref={panelRef}
      className="notifications-popover activity-popover"
      role="menu"
      onMouseDown={(e) => e.stopPropagation()}
    >
      <div className="activity-header">ACTIVITY · TODAY</div>
      {loading && summary == null ? (
        <div className="activity-empty">Loading…</div>
      ) : error ? (
        <div className="activity-empty">Couldn't load activity.</div>
      ) : isEmpty ? (
        <div className="activity-empty">No activity yet today.</div>
      ) : (
        <>
          {rows.length > 0 && (
            <ul className="activity-rows">
              {rows.map((r, i) => (
                <li key={i} className="activity-row">
                  {r}
                </li>
              ))}
            </ul>
          )}
          {events.length > 0 && (
            <>
              <div className="activity-section-head">RECENT</div>
              <ul className="activity-event-list">
                {events.map((ev) => (
                  <li
                    key={`${ev.ts_ms}-${ev.kind}-${ev.ref_id}`}
                    className="activity-event-row"
                  >
                    <button
                      type="button"
                      className="activity-event-btn"
                      onClick={() => {
                        onOpenObservation(
                          ev.actor_id,
                          ev.kind === "observation_accepted" ? ev.ref_id : null,
                        );
                        onClose();
                      }}
                    >
                      <div className="activity-event-meta">
                        {formatTime(ev.ts_ms)} · {labelFor(ev)}
                      </div>
                      {ev.kind === "observation_accepted" && ev.body && (
                        <div className="activity-event-body">
                          "{truncate(ev.body, 80)}"
                        </div>
                      )}
                    </button>
                  </li>
                ))}
              </ul>
            </>
          )}
        </>
      )}
    </div>
  );
}

function labelFor(ev: ActivityEventRow): string {
  if (ev.kind === "observation_accepted") {
    return `Accepted observation about ${ev.actor_display_name}`;
  }
  return `${ev.actor_display_name}'s profile updated`;
}

function formatTime(ts_ms: number): string {
  return new Date(ts_ms).toLocaleTimeString(undefined, {
    hour: "numeric",
    minute: "2-digit",
  });
}

function truncate(s: string, cap: number): string {
  if (s.length <= cap) return s;
  return s.slice(0, cap - 1).trimEnd() + "…";
}

function buildRows(s: DailyActivitySummary): string[] {
  const rows: string[] = [];

  if (s.emails_today > 0) {
    const noun = s.emails_today === 1 ? "email" : "emails";
    const actionable =
      s.emails_actionable > 0 ? ` (${s.emails_actionable} actionable)` : "";
    rows.push(`${s.emails_today} ${noun} today${actionable}`);
  }

  if (s.teams_messages_today > 0) {
    const noun = s.teams_messages_today === 1 ? "Teams message" : "Teams messages";
    rows.push(`${s.teams_messages_today} ${noun} today`);
  }

  if (s.meetings_held > 0 && s.meetings_upcoming > 0) {
    const heldNoun = s.meetings_held === 1 ? "meeting" : "meetings";
    rows.push(
      `${s.meetings_held} ${heldNoun} held, ${s.meetings_upcoming} to go`,
    );
  } else if (s.meetings_held > 0) {
    const noun = s.meetings_held === 1 ? "meeting" : "meetings";
    rows.push(`${s.meetings_held} ${noun} held`);
  } else if (s.meetings_upcoming > 0) {
    const noun = s.meetings_upcoming === 1 ? "meeting" : "meetings";
    rows.push(`${s.meetings_upcoming} ${noun} to go`);
  }

  if (s.meetings_missing_note > 0) {
    const noun = s.meetings_missing_note === 1 ? "meeting" : "meetings";
    rows.push(`${s.meetings_missing_note} ${noun} without a note`);
  }

  if (s.people_interacted > 0) {
    const noun = s.people_interacted === 1 ? "person" : "people";
    rows.push(`${s.people_interacted} ${noun} interacted with today`);
  }

  // Open questions (#113): all-time, not today-scoped. The other rows
  // are activity windows; this one is the standing backlog.
  if (s.open_questions_count > 0) {
    const noun = s.open_questions_count === 1 ? "question" : "questions";
    rows.push(`${s.open_questions_count} open ${noun}`);
  }

  return rows;
}
