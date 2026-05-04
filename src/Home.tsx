import { useEffect, useMemo, useState } from "react";

import { listMeetings, type MeetingItem } from "./file";

type Props = {
  recentFiles: string[];
  onOpen: (path: string) => void;
  onNewNote: () => void;
  onNewMeeting: () => void;
};

export function Home({ recentFiles, onOpen, onNewNote, onNewMeeting }: Props) {
  const [meetings, setMeetings] = useState<MeetingItem[]>([]);
  const [loading, setLoading] = useState<boolean>(true);

  useEffect(() => {
    let alive = true;
    listMeetings()
      .then((items) => {
        if (alive) setMeetings(items);
      })
      .catch((err) => console.error("listMeetings failed:", err))
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, []);

  const grouped = useMemo(() => groupByDay(meetings), [meetings]);

  return (
    <div className="home-scroll">
      <div className="home-inner">
        <header className="home-header">
          <div className="home-title">
            <h1>Margin</h1>
            <p className="home-subtitle">Your notes and meetings.</p>
          </div>
          <div className="home-cta">
            <button className="home-btn home-btn-secondary" onClick={onNewNote}>
              + New note
            </button>
            <button className="home-btn home-btn-primary" onClick={onNewMeeting}>
              + New meeting
            </button>
          </div>
        </header>

        <section className="home-section">
          <h2 className="home-section-title">Meetings</h2>
          {loading ? (
            <p className="home-empty">Loading…</p>
          ) : meetings.length === 0 ? (
            <p className="home-empty">
              No meetings yet — press <kbd>⌘⇧M</kbd> to start one.
            </p>
          ) : (
            <div className="home-meetings">
              {[...grouped.entries()].map(([dayKey, items]) => (
                <div key={dayKey} className="home-day-group">
                  <div className="home-day-heading">{formatDayHeading(dayKey)}</div>
                  <div className="home-day-cards">
                    {items.map((m) => (
                      <button
                        key={m.path}
                        className="home-card"
                        onClick={() => onOpen(m.path)}
                      >
                        <div className="home-card-row">
                          <span className="home-card-title">{m.title || "Untitled meeting"}</span>
                          <span className="home-card-time">{formatTime(m.modified_ms)}</span>
                        </div>
                        <div className="home-card-meta">
                          {formatDuration(m.duration_ms)}
                          {m.duration_ms !== null && " · transcribed"}
                        </div>
                      </button>
                    ))}
                  </div>
                </div>
              ))}
            </div>
          )}
        </section>

        {recentFiles.length > 0 && (
          <section className="home-section">
            <h2 className="home-section-title">Recent files</h2>
            <ul className="home-recents">
              {recentFiles.map((p) => (
                <li key={p}>
                  <button className="home-recent-row" onClick={() => onOpen(p)}>
                    <span className="home-recent-name">{filename(p)}</span>
                    <span className="home-recent-path">{prettyPath(p)}</span>
                  </button>
                </li>
              ))}
            </ul>
          </section>
        )}
      </div>
    </div>
  );
}

// ---------- helpers -------------------------------------------------------

function dayKey(ms: number): string {
  const d = new Date(ms);
  // YYYY-MM-DD in local time
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function groupByDay(items: MeetingItem[]): Map<string, MeetingItem[]> {
  const out = new Map<string, MeetingItem[]>();
  for (const item of items) {
    const k = dayKey(item.modified_ms);
    const list = out.get(k);
    if (list) list.push(item);
    else out.set(k, [item]);
  }
  return out;
}

function formatTime(ms: number): string {
  return new Date(ms).toLocaleTimeString(undefined, {
    hour: "numeric",
    minute: "2-digit",
  });
}

function formatDuration(ms: number | null): string {
  if (ms === null) return "—";
  const totalSec = Math.round(ms / 1000);
  if (totalSec < 60) return `${totalSec}s`;
  const min = Math.round(totalSec / 60);
  if (min < 60) return `${min} min`;
  const h = Math.floor(min / 60);
  const remMin = min % 60;
  return `${h}h ${remMin}m`;
}

function formatDayHeading(key: string): string {
  // key is YYYY-MM-DD local
  const [y, m, d] = key.split("-").map(Number);
  const date = new Date(y, m - 1, d);
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const yesterday = new Date(today);
  yesterday.setDate(today.getDate() - 1);
  if (date.getTime() === today.getTime()) return "Today";
  if (date.getTime() === yesterday.getTime()) return "Yesterday";
  return date.toLocaleDateString(undefined, {
    weekday: "short",
    month: "short",
    day: "numeric",
  });
}

function filename(path: string): string {
  return path.split("/").pop() ?? path;
}

function prettyPath(path: string): string {
  const home = "/Users/" + (path.split("/")[2] ?? "");
  return path.startsWith(home) ? "~" + path.slice(home.length) : path;
}
