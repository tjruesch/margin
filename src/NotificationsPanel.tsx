import { useEffect, useMemo, useRef } from "react";

import { IconSparkle } from "./icons";
import {
  type NotificationKind,
  type NotificationRecord,
} from "./notifications";

type Props = {
  open: boolean;
  notifications: NotificationRecord[];
  onClose: () => void;
  onOpenNote: (path: string) => void;
};

export function NotificationsPanel({
  open,
  notifications,
  onClose,
  onOpenNote,
}: Props) {
  const panelRef = useRef<HTMLDivElement | null>(null);

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

  const groups = useMemo(() => groupByDay(notifications), [notifications]);

  if (!open) return null;

  return (
    <div
      ref={panelRef}
      className="notifications-popover"
      role="menu"
      onMouseDown={(e) => e.stopPropagation()}
    >
      {notifications.length === 0 ? (
        <div className="notifications-empty">No notifications yet.</div>
      ) : (
        <>
          {groups.today.length > 0 && (
            <NotificationGroup
              label="Today"
              items={groups.today}
              onOpenNote={onOpenNote}
              onClose={onClose}
            />
          )}
          {groups.earlier.length > 0 && (
            <NotificationGroup
              label="Earlier"
              items={groups.earlier}
              onOpenNote={onOpenNote}
              onClose={onClose}
            />
          )}
        </>
      )}
    </div>
  );
}

function NotificationGroup({
  label,
  items,
  onOpenNote,
  onClose,
}: {
  label: string;
  items: NotificationRecord[];
  onOpenNote: (path: string) => void;
  onClose: () => void;
}) {
  return (
    <div className="notifications-group">
      <div className="notifications-group-label">{label}</div>
      {items.map((n) => (
        <button
          key={n.id}
          type="button"
          role="menuitem"
          className={
            "notifications-row" + (n.read_at == null ? " unread" : "")
          }
          onClick={() => {
            onClose();
            onOpenNote(n.note_path);
          }}
        >
          <span className={"notifications-row-icon kind-" + n.kind}>
            {iconFor(n.kind)}
          </span>
          <span className="notifications-row-body">
            <span className="notifications-row-title">
              {titleFor(n.kind, n.note_title)}
            </span>
            {n.body && (
              <span className="notifications-row-text">{n.body}</span>
            )}
          </span>
          <span className="notifications-row-time" title={absoluteLabel(n.created_ms)}>
            {relativeLabel(n.created_ms)}
          </span>
        </button>
      ))}
    </div>
  );
}

function iconFor(_kind: NotificationKind) {
  return <IconSparkle size={11} sw={1.7} />;
}

function titleFor(kind: NotificationKind, noteTitle: string): string {
  if (kind === "transcription-complete") return `Transcript ready: ${noteTitle}`;
  return `Notes generated: ${noteTitle}`;
}

function groupByDay(list: NotificationRecord[]): {
  today: NotificationRecord[];
  earlier: NotificationRecord[];
} {
  const startOfToday = new Date();
  startOfToday.setHours(0, 0, 0, 0);
  const cutoff = startOfToday.getTime();
  const today: NotificationRecord[] = [];
  const earlier: NotificationRecord[] = [];
  for (const n of list) {
    if (n.created_ms >= cutoff) today.push(n);
    else earlier.push(n);
  }
  return { today, earlier };
}

function relativeLabel(ms: number): string {
  const delta = Date.now() - ms;
  if (delta < 60_000) return "just now";
  if (delta < 3_600_000) return `${Math.floor(delta / 60_000)}m ago`;
  if (delta < 86_400_000) return `${Math.floor(delta / 3_600_000)}h ago`;
  if (delta < 86_400_000 * 2) return "yesterday";
  if (delta < 86_400_000 * 7) {
    return `${Math.floor(delta / 86_400_000)}d ago`;
  }
  return new Date(ms).toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
  });
}

function absoluteLabel(ms: number): string {
  return new Date(ms).toLocaleString();
}
