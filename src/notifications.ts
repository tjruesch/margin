// In-app notification records (#37). Two sources today:
//   - transcription-complete: a meeting's whisper pass finished.
//   - reconcile-complete:    the LLM produced reconciled notes.
//
// Persisted via tauri-plugin-store (`notifications.json`) so the queue
// survives app restarts. Capped to MAX_RECORDS — when capacity is hit,
// the oldest records are evicted from the tail.

import { LazyStore } from "@tauri-apps/plugin-store";

export type NotificationKind =
  | "transcription-complete"
  | "reconcile-complete";

export type NotificationRecord = {
  id: string;
  kind: NotificationKind;
  /** Source note path. The panel routes click-through to `loadFile`
   *  with this value. */
  note_path: string;
  note_title: string;
  /** Pre-formatted body line for display. Optional — kinds with
   *  self-explanatory titles can omit it. */
  body?: string;
  created_ms: number;
  /** Unix-ms when the user opened the panel after this record was
   *  added. Undefined = unread. */
  read_at?: number;
};

const STORE_FILE = "notifications.json";
const KEY = "list";
const MAX_RECORDS = 50;

let store: LazyStore | null = null;

function getStore(): LazyStore {
  if (!store) store = new LazyStore(STORE_FILE);
  return store;
}

export async function loadNotifications(): Promise<NotificationRecord[]> {
  try {
    const raw = await getStore().get<unknown>(KEY);
    if (!Array.isArray(raw)) return [];
    return raw.filter(isNotificationRecord);
  } catch (err) {
    console.error("loadNotifications failed:", err);
    return [];
  }
}

export async function saveNotifications(
  list: NotificationRecord[],
): Promise<void> {
  try {
    const s = getStore();
    await s.set(KEY, list);
    await s.save();
  } catch (err) {
    console.error("saveNotifications failed:", err);
  }
}

/** Prepend a record, cap to MAX_RECORDS, return the new list. */
export function pushNotification(
  list: NotificationRecord[],
  rec: NotificationRecord,
): NotificationRecord[] {
  return [rec, ...list].slice(0, MAX_RECORDS);
}

/** Stamp `read_at` on every unread record. Returns a new list when at
 *  least one record changed; the same array reference otherwise so
 *  callers can short-circuit a re-render. */
export function markAllRead(
  list: NotificationRecord[],
): NotificationRecord[] {
  const now = Date.now();
  let changed = false;
  const next = list.map((n) => {
    if (n.read_at == null) {
      changed = true;
      return { ...n, read_at: now };
    }
    return n;
  });
  return changed ? next : list;
}

export function unreadCount(list: NotificationRecord[]): number {
  let n = 0;
  for (const r of list) if (r.read_at == null) n++;
  return n;
}

/** Stable id for new records. Falls back to a small random suffix on
 *  the rare environment without crypto.randomUUID. */
export function makeNotificationId(): string {
  const c = (globalThis as { crypto?: { randomUUID?: () => string } }).crypto;
  if (c?.randomUUID) return c.randomUUID();
  return (
    Date.now().toString(36) + Math.random().toString(36).slice(2, 10)
  );
}

function isNotificationRecord(v: unknown): v is NotificationRecord {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  if (typeof o.id !== "string") return false;
  if (
    o.kind !== "transcription-complete" &&
    o.kind !== "reconcile-complete"
  ) {
    return false;
  }
  if (typeof o.note_path !== "string") return false;
  if (typeof o.note_title !== "string") return false;
  if (typeof o.created_ms !== "number") return false;
  return true;
}
