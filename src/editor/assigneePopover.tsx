// Portal popover that opens when the inline assignee chip in the
// CodeMirror editor is clicked. Mirrors the dueDatePopover pattern:
// listens for `margin:edit-assignee` CustomEvents, positions itself
// below the chip (flips above on overflow), shows the team-member list
// + Unassign, dispatches the parent's `onPick` callback on selection.
//
// `onPick` rounds-trips through `setActionAssignee` (#51) which rewrites
// the markdown body line on disk; the file watcher reloads the editor
// content so the chip and text stay in sync.

import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";

import type { TeamMember } from "../file";
import { avatarColor, initialsFromName } from "../initials";
import type { EditAssigneeDetail } from "./assigneeChip";

type Props = {
  members: TeamMember[];
  /** Called when the user picks a member or "Unassign". The handler is
   *  expected to dispatch `setActionAssignee` and refresh state so the
   *  chip's appearance updates on the next render. */
  onPick: (actionId: string, memberId: string | null) => Promise<void>;
};

type Active = (EditAssigneeDetail & { currentAssigneeId: string | null }) | null;

const MARGIN = 8;

export function AssigneePopover({ members, onPick }: Props) {
  const [active, setActive] = useState<Active>(null);
  const popoverRef = useRef<HTMLDivElement | null>(null);
  // Measured position. The `for` field pins each measurement to a
  // specific `active` instance so we can detect a stale position on
  // the first render after a new chip click and keep the popover
  // hidden until the layout effect re-measures.
  const [pos, setPos] = useState<
    { top: number; left: number; for: Active } | null
  >(null);

  // Single layout pass: measure the rendered popover and set its final
  // coords. Runs synchronously after the DOM is committed, so the user
  // never sees an unpositioned flash.
  useLayoutEffect(() => {
    if (!active) {
      setPos(null);
      return;
    }
    const el = popoverRef.current;
    if (!el) return;
    const popH = el.offsetHeight;
    const popW = el.offsetWidth;

    let top = active.rect.bottom + MARGIN;
    // Flip above when the popover would overflow the bottom of the
    // viewport. Uses the measured height, not an estimate, so the gap
    // between chip and popover stays tight.
    if (top + popH > window.innerHeight - MARGIN) {
      top = Math.max(MARGIN, active.rect.top - popH - MARGIN);
    }
    // Default left-anchor; right-anchor on horizontal overflow so the
    // menu stays close to the chip rather than drifting left.
    let left = active.rect.left;
    if (left + popW > window.innerWidth - MARGIN) {
      left = Math.max(MARGIN, active.rect.right - popW);
    }
    if (left < MARGIN) left = MARGIN;
    setPos({ top, left, for: active });
  }, [active, members]);

  // Subscribe to chip-click events from assigneeChip.ts.
  useEffect(() => {
    const onEdit = (e: Event) => {
      const detail = (e as CustomEvent<EditAssigneeDetail>).detail;
      // The chip's DOM doesn't carry the current assignee id, only the
      // action id — fish it back out of the DOM `style.background` is
      // brittle. Easier: leave currentAssigneeId null at open time;
      // the popover doesn't need to highlight a row to function. If we
      // want highlighting, the click handler can stash it in dataset.
      setActive({ ...detail, currentAssigneeId: null });
    };
    document.addEventListener("margin:edit-assignee", onEdit as EventListener);
    return () =>
      document.removeEventListener(
        "margin:edit-assignee",
        onEdit as EventListener,
      );
  }, []);

  // Outside-click and Escape dismissal.
  useEffect(() => {
    if (!active) return;
    const onMouseDown = (e: MouseEvent) => {
      const target = e.target as Node | null;
      if (popoverRef.current && target && popoverRef.current.contains(target)) {
        return;
      }
      setActive(null);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setActive(null);
    };
    window.addEventListener("mousedown", onMouseDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onMouseDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [active]);

  const sortedMembers = useMemo(() => {
    return [...members].sort((a, b) => {
      if (a.is_self !== b.is_self) return a.is_self ? -1 : 1;
      return a.display_name.localeCompare(b.display_name, undefined, {
        sensitivity: "base",
      });
    });
  }, [members]);

  if (!active) return null;

  const handlePick = (memberId: string | null) => {
    const actionId = active.actionId;
    setActive(null);
    void onPick(actionId, memberId);
  };

  // Pre-measure render: portal off-screen with visibility hidden so
  // the user doesn't see an unpositioned flash. The useLayoutEffect
  // above sets `pos` synchronously after first paint. We treat pos as
  // stale when it was measured for a previous `active` instance.
  const measured = pos !== null && pos.for === active;
  const style: React.CSSProperties = measured
    ? { top: pos.top, left: pos.left }
    : { top: -9999, left: -9999, visibility: "hidden" };

  return createPortal(
    <div
      ref={popoverRef}
      className="assignee-popover assignee-popover-floating"
      role="menu"
      style={style}
      onMouseDown={(e) => e.stopPropagation()}
    >
      {sortedMembers.length === 0 ? (
        <div className="assignee-popover-empty">No team members yet.</div>
      ) : (
        sortedMembers.map((m) => (
          <button
            key={m.id}
            type="button"
            role="menuitem"
            className="assignee-popover-row"
            onClick={() => handlePick(m.id)}
          >
            <span
              className="assignee-popover-disc"
              style={{ background: avatarColor(m.id) }}
            >
              {initialsFromName(m.display_name)}
            </span>
            <span className="assignee-popover-name">{m.display_name}</span>
            {m.is_self && <span className="assignee-popover-self">You</span>}
          </button>
        ))
      )}
      <div className="assignee-popover-sep" />
      <button
        type="button"
        role="menuitem"
        className="assignee-popover-row assignee-popover-unassign"
        onClick={() => handlePick(null)}
      >
        Unassign
      </button>
    </div>,
    document.body,
  );
}
