import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";

import {
  type TeamMember,
  getMeetingAttendees,
  listTeamMembers,
} from "./file";
import { IconPlus } from "./icons";
import { avatarColor, initialsFromName } from "./initials";

type Props = {
  notePath: string;
  onSubmit: (memberIds: string[]) => void;
  onCancel: () => void;
  onAddTeamMember: () => void;
};

export function AttendeePicker({
  notePath,
  onSubmit,
  onCancel,
  onAddTeamMember,
}: Props) {
  const [members, setMembers] = useState<TeamMember[]>([]);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [loaded, setLoaded] = useState(false);
  const cardRef = useRef<HTMLDivElement>(null);

  // Initial load: members + this meeting's previously-saved attendees.
  // Default selection prefers saved attendees; falls back to just Self.
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const [all, saved] = await Promise.all([
          listTeamMembers(),
          getMeetingAttendees(notePath),
        ]);
        if (cancelled) return;
        setMembers(all);
        if (saved.length > 0) {
          setSelected(new Set(saved.map((m) => m.id)));
        } else {
          const self = all.find((m) => m.is_self);
          setSelected(new Set(self ? [self.id] : []));
        }
        setLoaded(true);
      } catch (err) {
        console.error("AttendeePicker load failed:", err);
        if (!cancelled) setLoaded(true);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [notePath]);

  // Escape and Cmd/Ctrl+Enter shortcuts. Mirror the click-outside pattern
  // by checking the cardRef rather than capturing every event.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onCancel();
        return;
      }
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        if (selected.size > 0) onSubmit(Array.from(selected));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onCancel, onSubmit, selected]);

  const sortedMembers = useMemo(() => {
    // Backend already orders Self-first then alphabetical, but be
    // defensive in case future callers don't.
    return [...members].sort((a, b) => {
      if (a.is_self !== b.is_self) return a.is_self ? -1 : 1;
      return a.display_name.localeCompare(b.display_name, undefined, {
        sensitivity: "base",
      });
    });
  }, [members]);

  const toggle = useCallback((id: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  const onBackdropMouseDown = (e: React.MouseEvent) => {
    if (cardRef.current && cardRef.current.contains(e.target as Node)) return;
    onCancel();
  };

  return createPortal(
    <div
      className="attendee-modal-backdrop"
      role="dialog"
      aria-modal="true"
      aria-labelledby="attendee-modal-title"
      onMouseDown={onBackdropMouseDown}
    >
      <div
        ref={cardRef}
        className="attendee-modal-card"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <header className="attendee-modal-head">
          <h2 id="attendee-modal-title" className="attendee-modal-title">
            Who attended this meeting?
          </h2>
          <p className="attendee-modal-sub">
            Action items will be attributed to listed attendees by name.
          </p>
        </header>
        <div className="attendee-modal-list">
          {!loaded ? (
            <div className="attendee-modal-empty">Loading…</div>
          ) : sortedMembers.length === 0 ? (
            <div className="attendee-modal-empty">
              No team members yet. Add one to get started.
            </div>
          ) : (
            sortedMembers.map((m) => {
              const checked = selected.has(m.id);
              return (
                <label
                  key={m.id}
                  className={"attendee-modal-row" + (checked ? " checked" : "")}
                >
                  <input
                    type="checkbox"
                    className="attendee-modal-check"
                    checked={checked}
                    onChange={() => toggle(m.id)}
                  />
                  <span
                    className="attendee-modal-avatar"
                    style={{ background: avatarColor(m.id) }}
                  >
                    {initialsFromName(m.display_name)}
                  </span>
                  <span className="attendee-modal-row-body">
                    <span className="attendee-modal-row-name">
                      {m.display_name}
                      {m.is_self && (
                        <span className="attendee-modal-self">You</span>
                      )}
                    </span>
                    {m.role && (
                      <span className="attendee-modal-row-role">{m.role}</span>
                    )}
                  </span>
                </label>
              );
            })
          )}
        </div>
        <button
          type="button"
          className="attendee-modal-add"
          onClick={onAddTeamMember}
        >
          <IconPlus size={12} sw={1.8} />
          Add team member
        </button>
        <footer className="attendee-modal-footer">
          <button type="button" className="ghost" onClick={onCancel}>
            Cancel
          </button>
          <button
            type="button"
            className="attendee-modal-submit"
            disabled={selected.size === 0}
            onClick={() => onSubmit(Array.from(selected))}
          >
            Generate notes
          </button>
        </footer>
      </div>
    </div>,
    document.body,
  );
}
