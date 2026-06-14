import { useEffect, useMemo, useRef, useState } from "react";

import { type TeamMember } from "./file";
import { IconPlus } from "./icons";
import { avatarColor, initialsFromName } from "./initials";

type Props = {
  assigneeId: string | null;
  assigneeDisplayName: string | null;
  members: TeamMember[];
  onPick: (memberId: string | null) => void;
};

/// Strip a leading "Owner — " segment from row text — TS counterpart
/// of the Rust `strip_leading_owner_segment` (#51). Used to hide the
/// prefix in the displayed text once the assignee is shown via the
/// chip. Pure render-time transform; doesn't touch the body.
export function stripLeadingOwnerPrefix(text: string): string {
  const seps = [" — ", " – ", " -- "];
  let best: { idx: number; len: number } | null = null;
  for (const sep of seps) {
    const idx = text.indexOf(sep);
    if (idx <= 0) continue;
    if (text.slice(0, idx).trim().length === 0) continue;
    if (best === null || idx < best.idx) best = { idx, len: sep.length };
  }
  return best === null ? text : text.slice(best.idx + best.len);
}

export function AssigneeChip({
  assigneeId,
  assigneeDisplayName,
  members,
  onPick,
}: Props) {
  const [open, setOpen] = useState(false);
  const anchorRef = useRef<HTMLDivElement | null>(null);

  // Outside-click + Escape dismissal, mirrors MoreMenu.
  useEffect(() => {
    if (!open) return;
    const onMouseDown = (e: MouseEvent) => {
      const target = e.target as Node | null;
      if (anchorRef.current && target && anchorRef.current.contains(target)) return;
      setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    window.addEventListener("mousedown", onMouseDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onMouseDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [open]);

  const sortedMembers = useMemo(() => {
    // Self pinned, then alphabetical (case-insensitive). Backend
    // already orders this way, but be defensive.
    return [...members].sort((a, b) => {
      if (a.is_self !== b.is_self) return a.is_self ? -1 : 1;
      return a.display_name.localeCompare(b.display_name, undefined, {
        sensitivity: "base",
      });
    });
  }, [members]);

  const handlePick = (memberId: string | null) => {
    setOpen(false);
    onPick(memberId);
  };

  return (
    <div
      className="assignee-popover-anchor"
      ref={anchorRef}
      onClick={(e) => {
        // Action rows have an outer click handler that opens the source
        // note. Stop propagation so clicking the chip doesn't navigate.
        e.stopPropagation();
      }}
      onKeyDown={(e) => {
        // Same idea for keyboard activation on the row.
        e.stopPropagation();
      }}
    >
      {assigneeId && assigneeDisplayName ? (
        <button
          type="button"
          className="assignee-chip"
          aria-label={`Assigned to ${assigneeDisplayName}; click to change`}
          aria-haspopup="menu"
          aria-expanded={open}
          title={assigneeDisplayName}
          style={{ background: avatarColor(assigneeId) }}
          onClick={() => setOpen((v) => !v)}
        >
          {initialsFromName(assigneeDisplayName)}
        </button>
      ) : (
        <button
          type="button"
          className="assignee-chip-empty"
          aria-label="Unassigned; click to assign"
          aria-haspopup="menu"
          aria-expanded={open}
          title="Assign…"
          onClick={() => setOpen((v) => !v)}
        >
          <IconPlus size={11} sw={2} />
        </button>
      )}
      {open && (
        <div className="assignee-popover" role="menu">
          {sortedMembers.length === 0 ? (
            <div className="assignee-popover-empty">No team members yet.</div>
          ) : (
            sortedMembers.map((m) => {
              const active = m.id === assigneeId;
              return (
                <button
                  key={m.id}
                  type="button"
                  role="menuitem"
                  className={
                    "assignee-popover-row" + (active ? " active" : "")
                  }
                  onClick={() => handlePick(m.id)}
                >
                  <span
                    className="assignee-popover-disc"
                    style={{ background: avatarColor(m.id) }}
                  >
                    {initialsFromName(m.display_name)}
                  </span>
                  <span className="assignee-popover-name">
                    {m.display_name}
                  </span>
                  {m.is_self && (
                    <span className="assignee-popover-self">You</span>
                  )}
                </button>
              );
            })
          )}
          {assigneeId && (
            <>
              <div className="assignee-popover-sep" />
              <button
                type="button"
                role="menuitem"
                className="assignee-popover-row assignee-popover-unassign"
                onClick={() => handlePick(null)}
              >
                Unassign
              </button>
            </>
          )}
        </div>
      )}
    </div>
  );
}
