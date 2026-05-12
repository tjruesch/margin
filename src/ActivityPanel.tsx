import { useEffect, useRef, useState } from "react";

import { getDailyActivity, type DailyActivitySummary } from "./file";

type Props = {
  open: boolean;
  onClose: () => void;
};

export function ActivityPanel({ open, onClose }: Props) {
  const panelRef = useRef<HTMLDivElement | null>(null);
  const [summary, setSummary] = useState<DailyActivitySummary | null>(null);
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
    getDailyActivity()
      .then((s) => {
        if (!cancelled) setSummary(s);
      })
      .catch((e) => {
        if (!cancelled) setError(String(e));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [open]);

  if (!open) return null;

  const rows = summary ? buildRows(summary) : [];
  const isEmpty = !loading && !error && summary != null && rows.length === 0;

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
        <ul className="activity-rows">
          {rows.map((r, i) => (
            <li key={i} className="activity-row">
              {r}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
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

  return rows;
}
