// Shared friendly-label and bucket logic for due-date chips. Used by the
// Home page (Todo feed) and the inline editor chip so both surfaces show
// identical wording and color buckets.

const WEEKDAY_SHORT = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTH_SHORT = [
  "Jan", "Feb", "Mar", "Apr", "May", "Jun",
  "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/** Calendar-day index in the local timezone — increments by 1 each
 *  midnight regardless of DST. Comparing two ms timestamps via this
 *  function returns "calendar days apart in the user's timezone". */
function localDayIndex(ms: number): number {
  const d = new Date(ms);
  return Math.floor(
    (d.getTime() - d.getTimezoneOffset() * 60_000) / 86_400_000,
  );
}

function pad2(n: number): string {
  return n < 10 ? `0${n}` : `${n}`;
}

export type DueBucket = "overdue" | "today" | "soon" | "later";

export function dueBucket(dueMs: number, nowMs: number): DueBucket {
  const delta = localDayIndex(dueMs) - localDayIndex(nowMs);
  if (delta < 0) return "overdue";
  if (delta === 0) return "today";
  if (delta <= 7) return "soon";
  return "later";
}

/** Friendly chip label for `dueMs` against `nowMs`. Includes a `HH:MM`
 *  suffix when the underlying timestamp lands at a non-midnight local
 *  time. */
export function friendlyDueLabel(dueMs: number, nowMs: number): string {
  const due = new Date(dueMs);
  const now = new Date(nowMs);
  const delta = localDayIndex(dueMs) - localDayIndex(nowMs);
  const hasTime = due.getHours() !== 0 || due.getMinutes() !== 0;
  const timeSuffix = hasTime ? ` ${pad2(due.getHours())}:${pad2(due.getMinutes())}` : "";

  let body: string;
  if (delta === 0) body = "Today";
  else if (delta === 1) body = "Tomorrow";
  else if (delta === -1) body = "Yesterday";
  else if (delta > 1 && delta <= 6) body = WEEKDAY_SHORT[due.getDay()];
  else if (due.getFullYear() === now.getFullYear()) {
    body = `${MONTH_SHORT[due.getMonth()]} ${due.getDate()}`;
  } else {
    const yy = String(due.getFullYear()).slice(-2);
    body = `${MONTH_SHORT[due.getMonth()]} ${due.getDate()} '${yy}`;
  }
  return body + timeSuffix;
}

/** Parse `YYYY-MM-DD` or `YYYY-MM-DD HH:MM` (the on-disk canonical token
 *  forms) to local-tz Unix ms. Returns `null` for any other shape. */
export function parseAbsoluteToken(token: string): number | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})(?:\s(\d{2}):(\d{2}))?$/.exec(token);
  if (!m) return null;
  const y = Number(m[1]);
  const mo = Number(m[2]) - 1;
  const d = Number(m[3]);
  const hh = m[4] != null ? Number(m[4]) : 0;
  const mm = m[5] != null ? Number(m[5]) : 0;
  if (mo < 0 || mo > 11 || d < 1 || d > 31 || hh > 23 || mm > 59) return null;
  const date = new Date(y, mo, d, hh, mm, 0, 0);
  if (date.getFullYear() !== y || date.getMonth() !== mo || date.getDate() !== d) {
    return null;
  }
  return date.getTime();
}
