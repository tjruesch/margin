// Date-picker popover that opens when the user clicks an inline due-date
// chip in the editor. Listens for the `margin:edit-due` CustomEvent
// dispatched by `dueDateChip.ts` and mounts a small DOM-portal popover
// anchored to the chip's bounding rect. Submit dispatches a CodeMirror
// transaction that replaces the trailing `@<token>` range with the new
// absolute token (or deletes it entirely on "Clear").

import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import type { EditorView } from "@codemirror/view";

import type { EditDueDetail } from "./dueDateChip";

type Active = EditDueDetail | null;

function pad2(n: number): string {
  return n < 10 ? `0${n}` : `${n}`;
}

function formatDateInputValue(ms: number): string {
  const d = new Date(ms);
  return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
}

function formatTimeInputValue(ms: number): string {
  const d = new Date(ms);
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
}

/** Whether the timestamp lands at local midnight — used to decide if the
 *  user previously had a time-of-day on the chip (so we default the
 *  "include time" toggle correctly). */
function hasLocalTime(ms: number): boolean {
  const d = new Date(ms);
  return d.getHours() !== 0 || d.getMinutes() !== 0;
}

function buildAbsoluteToken(dateStr: string, timeStr: string | null): string {
  // dateStr is `YYYY-MM-DD`; timeStr is `HH:MM` or null.
  return timeStr ? `${dateStr} ${timeStr}` : dateStr;
}

/** Add `days` to a `YYYY-MM-DD` string, returning a new `YYYY-MM-DD` in
 *  local time (so DST transitions don't shift the day). */
function addDays(dateStr: string, days: number): string {
  const [y, m, d] = dateStr.split("-").map(Number);
  const dt = new Date(y, m - 1, d + days);
  return `${dt.getFullYear()}-${pad2(dt.getMonth() + 1)}-${pad2(dt.getDate())}`;
}

const WEEKDAY_LONG = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

function todayStr(): string {
  return formatDateInputValue(Date.now());
}

/** Compute the date string for the next occurrence of weekday `target`
 *  (0=Sun..6=Sat). If today's name and we treat "next" as inclusive of
 *  today, returns today; matches the Rust parser's rule. */
function nextWeekdayStr(target: number): string {
  const now = new Date();
  let diff = target - now.getDay();
  if (diff < 0) diff += 7;
  return addDays(todayStr(), diff);
}

export function DueDatePopover() {
  const [active, setActive] = useState<Active>(null);
  const [dateStr, setDateStr] = useState<string>(todayStr());
  const [includeTime, setIncludeTime] = useState(false);
  const [timeStr, setTimeStr] = useState("09:00");
  const popoverRef = useRef<HTMLDivElement | null>(null);
  // Measured position. The `for` field pins each measurement to a
  // specific `active` instance so we can detect a stale position on
  // the first render after a new chip click and keep the popover
  // hidden until the layout effect re-measures.
  const [pos, setPos] = useState<
    { top: number; left: number; for: Active } | null
  >(null);

  // Single layout pass: measure the rendered popover and set its final
  // coords. Mirrors the assignee popover's pattern.
  useLayoutEffect(() => {
    if (!active) {
      setPos(null);
      return;
    }
    const el = popoverRef.current;
    if (!el) return;
    const popH = el.offsetHeight;
    const popW = el.offsetWidth;
    const margin = 8;
    let top = active.rect.bottom + margin;
    if (top + popH > window.innerHeight - margin) {
      top = Math.max(margin, active.rect.top - popH - margin);
    }
    let left = active.rect.left;
    if (left + popW > window.innerWidth - margin) {
      left = Math.max(margin, active.rect.right - popW);
    }
    if (left < margin) left = margin;
    setPos({ top, left, for: active });
  }, [active]);

  // Subscribe to chip-click events from dueDateChip.ts.
  useEffect(() => {
    const onEdit = (e: Event) => {
      const detail = (e as CustomEvent<EditDueDetail>).detail;
      setActive(detail);
      setDateStr(formatDateInputValue(detail.dueMs));
      setIncludeTime(hasLocalTime(detail.dueMs));
      setTimeStr(formatTimeInputValue(detail.dueMs));
    };
    document.addEventListener("margin:edit-due", onEdit as EventListener);
    return () => document.removeEventListener("margin:edit-due", onEdit as EventListener);
  }, []);

  // Outside-click and Escape dismissal. Mirrors the TagCluster popover
  // pattern in NoteHeader.tsx.
  useEffect(() => {
    if (!active) return;
    const onMouseDown = (e: MouseEvent) => {
      const target = e.target as Node | null;
      if (popoverRef.current && target && popoverRef.current.contains(target)) return;
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

  const quickOptions = useMemo(
    () => [
      { label: "Today", value: todayStr() },
      { label: "Tomorrow", value: addDays(todayStr(), 1) },
      // "Next <weekday>" three options for the rest of the week.
      ...[1, 2, 3].map((offset) => {
        const idx = (new Date().getDay() + offset + 1) % 7;
        return { label: WEEKDAY_LONG[idx], value: nextWeekdayStr(idx) };
      }),
    ],
    [active], // recompute each open so "Today" stays accurate
  );

  if (!active) return null;

  const replaceRange = (view: EditorView, from: number, to: number, insert: string) => {
    view.dispatch({ changes: { from, to, insert } });
    view.focus();
  };

  const submit = () => {
    if (!dateStr) return;
    const token = buildAbsoluteToken(dateStr, includeTime ? timeStr : null);
    // Replace just the @<token> range; the leading whitespace already on
    // disk stays in place.
    replaceRange(active.view, active.from, active.to, `@${token}`);
    setActive(null);
  };

  const clear = () => {
    // Delete the leading whitespace too so the line ends cleanly without
    // a dangling space after the action text.
    const from = Math.max(0, active.from - 1);
    replaceRange(active.view, from, active.to, "");
    setActive(null);
  };

  // Pre-measure render: place off-screen with visibility hidden so
  // the user doesn't see an unpositioned flash. We treat `pos` as
  // stale when it was measured for a previous `active` instance.
  const measured = pos !== null && pos.for === active;
  const style: React.CSSProperties = measured
    ? { top: pos.top, left: pos.left, width: 280 }
    : { top: -9999, left: -9999, width: 280, visibility: "hidden" };

  return createPortal(
    <div
      ref={popoverRef}
      className="due-popover"
      style={style}
      onMouseDown={(e) => e.stopPropagation()}
    >
      <div className="due-popover-quick">
        {quickOptions.map((opt) => (
          <button
            key={opt.label}
            type="button"
            className={"due-popover-quick-btn" + (opt.value === dateStr ? " active" : "")}
            onClick={() => setDateStr(opt.value)}
          >
            {opt.label}
          </button>
        ))}
      </div>
      <div className="due-popover-row">
        <label className="due-popover-label">Date</label>
        <input
          type="date"
          className="due-popover-input"
          value={dateStr}
          onChange={(e) => setDateStr(e.target.value)}
        />
      </div>
      <div className="due-popover-row">
        <label className="due-popover-label">
          <input
            type="checkbox"
            checked={includeTime}
            onChange={(e) => setIncludeTime(e.target.checked)}
          />
          Time
        </label>
        <input
          type="time"
          className="due-popover-input"
          value={timeStr}
          disabled={!includeTime}
          onChange={(e) => setTimeStr(e.target.value)}
        />
      </div>
      <div className="due-popover-actions">
        <button type="button" className="due-popover-clear" onClick={clear}>
          Clear
        </button>
        <div style={{ flex: 1 }} />
        <button
          type="button"
          className="due-popover-cancel"
          onClick={() => setActive(null)}
        >
          Cancel
        </button>
        <button type="button" className="due-popover-save" onClick={submit}>
          Save
        </button>
      </div>
    </div>,
    document.body,
  );
}
