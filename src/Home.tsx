import { useEffect, useMemo, useState } from "react";

import { type NoteListItem } from "./file";
import { MoreMenu } from "./MoreMenu";
import {
  IconArrowRight,
  IconBell,
  IconCalendar,
  IconCheck,
  IconChecklist,
  IconChevRight,
  IconFileText,
  IconHome,
  IconMic,
  IconMore,
  IconPlus,
  IconSearch,
  IconSettings,
  IconSidebar,
  IconSparkle,
  IconStar,
  IconUsers,
} from "./icons";

type Props = {
  recentFiles: string[];
  notes: NoteListItem[];
  notesLoading: boolean;
  allTags: string[];
  onOpen: (path: string) => void;
  onNewNote: () => void;
  onNewMeeting: () => void;
  onOpenSettings: () => void;
  onDeleteRow?: (path: string) => void;
};

type NavId = "home" | "actions" | "meetings" | "shared" | "favorites";
type FilterId = "all" | "notes" | "meetings" | "shared";

// Stub data for sections whose backends don't exist yet (#27, #28).
// These return empty arrays so the sections hide gracefully; swapping in
// live data later is a one-line change.
const UPCOMING_EVENTS: UpcomingEvent[] = [];
const ACTION_ITEMS: ActionItem[] = [];

export function Home({
  notes,
  notesLoading,
  allTags,
  onOpen,
  onNewNote,
  onNewMeeting,
  onOpenSettings,
  onDeleteRow,
}: Props) {
  const [nav, setNav] = useState<NavId>("home");
  const [filter, setFilter] = useState<FilterId>("all");
  const [tagFilter, setTagFilter] = useState<string | null>(null);
  const [sidebarOpen, setSidebarOpen] = useState<boolean>(() => {
    return typeof localStorage === "undefined"
      ? true
      : localStorage.getItem("home.sidebarOpen") !== "0";
  });

  useEffect(() => {
    if (typeof localStorage !== "undefined") {
      localStorage.setItem("home.sidebarOpen", sidebarOpen ? "1" : "0");
    }
  }, [sidebarOpen]);

  const filteredNotes = useMemo(() => {
    let list = notes;
    if (tagFilter) list = list.filter((n) => n.tags.includes(tagFilter));
    switch (filter) {
      case "notes":
        return list.filter((n) => n.duration_ms === null);
      case "meetings":
        return list.filter((n) => n.duration_ms !== null);
      case "shared":
        return [];
      default:
        return list;
    }
  }, [notes, filter, tagFilter]);

  const grouped = useMemo(() => groupByDay(filteredNotes), [filteredNotes]);

  const openActionCount = ACTION_ITEMS.filter((a) => !a.done).length;

  return (
    <div className={"home" + (sidebarOpen ? "" : " home-collapsed")}>
      {sidebarOpen && (
        <Sidebar
          active={nav}
          onSelect={setNav}
          actionCount={openActionCount}
          meetingCount={UPCOMING_EVENTS.length}
          tags={allTags}
          activeTag={tagFilter}
          onTagSelect={(t) => setTagFilter(t === tagFilter ? null : t)}
          onOpenSettings={onOpenSettings}
        />
      )}
      <div className="home-main">
        <div className="home-toolbar" data-tauri-drag-region>
          <button
            type="button"
            className="home-icon-btn"
            title={sidebarOpen ? "Hide sidebar" : "Show sidebar"}
            aria-label="Toggle sidebar"
            onClick={() => setSidebarOpen((v) => !v)}
          >
            <IconSidebar size={14} sw={1.6} />
          </button>
          <button
            type="button"
            className="home-icon-btn"
            title="Notifications — coming soon (issue #37)"
            aria-label="Notifications"
            onClick={() => stub("Notifications", 37)}
          >
            <IconBell size={14} sw={1.6} />
          </button>
        </div>

        <Greeting
          upcomingCount={UPCOMING_EVENTS.length}
          nextEvent={UPCOMING_EVENTS[0] ?? null}
          onNewNote={onNewNote}
          onNewMeeting={onNewMeeting}
        />

        {UPCOMING_EVENTS.length > 0 && <UpcomingStrip events={UPCOMING_EVENTS} />}

        {openActionCount > 0 && (
          <ActionItemsTeaser items={ACTION_ITEMS} />
        )}

        <NotesFeed
          loading={notesLoading}
          grouped={grouped}
          totalNotes={notes.length}
          filter={filter}
          onFilterChange={setFilter}
          tagFilter={tagFilter}
          onClearTagFilter={() => setTagFilter(null)}
          onOpen={onOpen}
          onDeleteRow={onDeleteRow}
        />

        <div className="home-spacer" />
        <AskBar />
      </div>
    </div>
  );
}

// ---------- Sidebar -------------------------------------------------------

function Sidebar({
  active,
  onSelect,
  actionCount,
  meetingCount,
  tags,
  activeTag,
  onTagSelect,
  onOpenSettings,
}: {
  active: NavId;
  onSelect: (id: NavId) => void;
  actionCount: number;
  meetingCount: number;
  tags: string[];
  activeTag: string | null;
  onTagSelect: (tag: string) => void;
  onOpenSettings: () => void;
}) {
  return (
    <aside className="home-sidebar">
      <div className="home-titlebar" data-tauri-drag-region />
      <div className="home-search-wrap">
        <div
          className="home-search"
          data-tauri-drag-region="false"
          title="Search — coming soon (issue #31)"
          onClick={() => stub("Search", 31)}
        >
          <IconSearch size={13} sw={1.8} />
          <span className="home-search-placeholder">Search notes…</span>
          <span className="home-search-kbd">⌘K</span>
        </div>
      </div>

      <nav className="home-nav">
        <NavItem
          icon={<IconHome size={14} sw={1.7} />}
          label="Home"
          active={active === "home"}
          onClick={() => onSelect("home")}
        />
        <NavItem
          icon={<IconChecklist size={14} sw={1.7} />}
          label="Action items"
          badge={actionCount > 0 ? String(actionCount) : null}
          active={active === "actions"}
          onClick={() => onSelect("actions")}
        />
        <NavItem
          icon={<IconCalendar size={14} sw={1.7} />}
          label="Meetings"
          badge={meetingCount > 0 ? String(meetingCount) : null}
          active={active === "meetings"}
          onClick={() => onSelect("meetings")}
        />
        <NavItem
          icon={<IconUsers size={14} sw={1.7} />}
          label="Shared with me"
          active={active === "shared"}
          onClick={() => onSelect("shared")}
        />
        <NavItem
          icon={<IconStar size={14} sw={1.7} />}
          label="Favorites"
          active={active === "favorites"}
          onClick={() => onSelect("favorites")}
        />
      </nav>

      <div className="home-side-section">
        <div className="home-side-header">
          <span>Tags</span>
        </div>
        <div className="home-side-tags">
          {tags.length === 0 ? (
            <span className="home-side-tags-empty">
              No tags yet. Add tags to your notes to show them here.
            </span>
          ) : (
            tags.map((t) => (
              <button
                key={t}
                type="button"
                className={"home-side-tag" + (activeTag === t ? " active" : "")}
                style={{ "--tag-dot": tagDotColor(t) } as React.CSSProperties}
                onClick={() => onTagSelect(t)}
                title={activeTag === t ? `Clear filter` : `Filter by ${t}`}
              >
                {t}
              </button>
            ))
          )}
        </div>
      </div>

      <div className="home-side-foot">
        <NavItem
          icon={<IconSettings size={14} sw={1.7} />}
          label="Settings"
          active={false}
          onClick={onOpenSettings}
        />
      </div>
    </aside>
  );
}

function NavItem({
  icon,
  label,
  active,
  badge,
  onClick,
}: {
  icon: React.ReactNode;
  label: string;
  active: boolean;
  badge?: string | null;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      className={"home-nav-item" + (active ? " active" : "")}
      onClick={onClick}
    >
      <span className="home-nav-icon">{icon}</span>
      <span className="home-nav-label">{label}</span>
      {badge && <span className="home-nav-badge">{badge}</span>}
    </button>
  );
}

// ---------- Greeting ------------------------------------------------------

function Greeting({
  upcomingCount,
  nextEvent,
  onNewNote,
  onNewMeeting,
}: {
  upcomingCount: number;
  nextEvent: UpcomingEvent | null;
  onNewNote: () => void;
  onNewMeeting: () => void;
}) {
  const now = new Date();
  const dateStr = now.toLocaleDateString(undefined, {
    weekday: "long",
    month: "long",
    day: "numeric",
  });
  const greeting = greetingFor(now.getHours());
  const displayName = inferDisplayName();

  return (
    <header className="home-greeting">
      <div className="home-greeting-text">
        <div className="home-greeting-eyebrow">{dateStr}</div>
        <h1 className="home-greeting-title">
          {greeting}
          {displayName ? `, ${displayName}` : ""}
        </h1>
        {upcomingCount > 0 && (
          <div className="home-greeting-sub">
            You have {upcomingCount} meeting{upcomingCount === 1 ? "" : "s"} today
            {nextEvent ? (
              <>
                {" "}and{" "}
                <span className="home-greeting-accent">
                  1 starts {nextEvent.when.toLowerCase()}
                </span>
              </>
            ) : (
              "."
            )}
          </div>
        )}
      </div>

      <div className="home-greeting-cta">
        <button type="button" className="home-cta-secondary" onClick={onNewNote}>
          <IconPlus size={13} sw={1.9} />
          New note
        </button>
        <button type="button" className="home-cta-primary" onClick={onNewMeeting}>
          <span className="home-cta-dot" />
          New meeting
        </button>
      </div>
    </header>
  );
}

// ---------- Upcoming meetings strip --------------------------------------

type UpcomingEvent = {
  id: string;
  when: string;
  tStart: string;
  tEnd: string;
  title: string;
  attendees: string[];
  tags: string[];
  color: string;
  live?: boolean;
};

function UpcomingStrip({ events }: { events: UpcomingEvent[] }) {
  return (
    <section className="home-section">
      <SectionTitle eyebrow="Upcoming" title="Coming up" actionLabel="View calendar" actionIssue={27} />
      <div className="home-upcoming">
        {events.map((ev) => (
          <button key={ev.id} type="button" className="home-upcoming-card">
            <span className="home-upcoming-strip" style={{ background: ev.color }} />
            <div className="home-upcoming-row">
              <span className={"home-upcoming-when" + (ev.live ? " live" : "")}>
                {ev.live && <span className="home-upcoming-pulse" />}
                {ev.when}
              </span>
              <span className="home-upcoming-time">
                {ev.tStart}–{ev.tEnd}
              </span>
            </div>
            <div className="home-upcoming-title">{ev.title}</div>
            <div className="home-upcoming-foot">
              <AttendeeStack attendees={ev.attendees} />
              <TagChips tags={ev.tags} max={2} />
            </div>
          </button>
        ))}
      </div>
    </section>
  );
}

function AttendeeStack({ attendees }: { attendees: string[] }) {
  return (
    <div className="home-attendees">
      {attendees.slice(0, 4).map((a, i) => (
        <span
          key={`${a}-${i}`}
          className="home-attendee"
          style={{ background: avatarColor(a) }}
        >
          {a}
        </span>
      ))}
      {attendees.length > 4 && (
        <span className="home-attendee home-attendee-more">+{attendees.length - 4}</span>
      )}
    </div>
  );
}

// ---------- Action items teaser ------------------------------------------

type ActionItem = {
  id: string;
  text: string;
  done: boolean;
  due?: string;
  dueState?: "today" | "soon" | "later";
  source: { kind: "note" | "meeting"; title: string };
  tags?: string[];
};

function ActionItemsTeaser({ items }: { items: ActionItem[] }) {
  const order = { today: 0, soon: 1, later: 2 } as const;
  const top = items
    .filter((it) => !it.done)
    .sort((a, b) => (order[a.dueState ?? "later"] ?? 3) - (order[b.dueState ?? "later"] ?? 3))
    .slice(0, 3);
  if (top.length === 0) return null;

  return (
    <section className="home-section">
      <SectionTitle
        eyebrow="Action items"
        title="Things to do"
        actionLabel={`View all (${items.filter((i) => !i.done).length})`}
        actionIssue={28}
      />
      <div className="home-actions">
        {top.map((it) => (
          <div key={it.id} className="home-action-row">
            <button
              type="button"
              className={"home-checkbox" + (it.done ? " done" : "")}
              aria-label={it.done ? "Mark as open" : "Mark as done"}
              onClick={() => stub("Toggle action", 28)}
            >
              {it.done && <IconCheck size={12} sw={2.6} />}
            </button>
            <div className="home-action-body">
              <div className={"home-action-text" + (it.done ? " done" : "")}>{it.text}</div>
              <div className="home-action-meta">
                <span className="home-action-source">
                  {it.source.kind === "meeting" ? (
                    <IconMic size={11} sw={1.7} />
                  ) : (
                    <IconFileText size={11} sw={1.7} />
                  )}
                  {it.source.title}
                </span>
                {it.due && (
                  <span className={"home-due " + (it.dueState ?? "later")}>{it.due}</span>
                )}
                <TagChips tags={it.tags ?? []} max={3} />
              </div>
            </div>
          </div>
        ))}
      </div>
    </section>
  );
}

// ---------- Notes feed ----------------------------------------------------

function NotesFeed({
  loading,
  grouped,
  totalNotes,
  filter,
  onFilterChange,
  tagFilter,
  onClearTagFilter,
  onOpen,
  onDeleteRow,
}: {
  loading: boolean;
  grouped: Map<string, NoteListItem[]>;
  totalNotes: number;
  filter: FilterId;
  onFilterChange: (id: FilterId) => void;
  tagFilter: string | null;
  onClearTagFilter: () => void;
  onOpen: (path: string) => void;
  onDeleteRow?: (path: string) => void;
}) {
  const filters: { id: FilterId; label: string }[] = [
    { id: "all", label: "All" },
    { id: "notes", label: "Notes" },
    { id: "meetings", label: "Meetings" },
    { id: "shared", label: "Shared" },
  ];

  return (
    <section className="home-section">
      <div className="home-section-head">
        <div>
          <div className="home-section-eyebrow">Library</div>
          <h2 className="home-section-title">Your notes</h2>
        </div>
        <div className="home-filter">
          {filters.map((f) => (
            <button
              key={f.id}
              type="button"
              className={"home-filter-chip" + (filter === f.id ? " active" : "")}
              onClick={() => onFilterChange(f.id)}
            >
              {f.label}
            </button>
          ))}
        </div>
      </div>

      {tagFilter && (
        <div className="home-active-filter">
          <span className="home-active-filter-label">Filtering by</span>
          <button
            type="button"
            className="home-active-filter-chip"
            style={{ background: tagColor(tagFilter) }}
            onClick={onClearTagFilter}
            title="Clear filter"
          >
            <span>{tagFilter}</span>
            <span className="home-active-filter-x" aria-hidden="true">×</span>
          </button>
        </div>
      )}

      {loading ? (
        <p className="home-empty">Loading…</p>
      ) : totalNotes === 0 ? (
        <p className="home-empty">
          No notes yet — press <kbd>⌘N</kbd> for a new note,{" "}
          <kbd>⌘⇧M</kbd> to start one with a recording.
        </p>
      ) : grouped.size === 0 ? (
        <p className="home-empty">
          {filter === "shared"
            ? "Sharing is coming soon — see issue #15 / #32."
            : "Nothing matches this filter."}
        </p>
      ) : (
        [...grouped.entries()].map(([dayKey, items]) => (
          <div key={dayKey} className="home-day-group">
            <div className="home-day-heading">
              <span>{formatDayHeading(dayKey)}</span>
              <span className="home-day-rule" />
              <span className="home-day-count">{items.length} items</span>
            </div>
            <div className="home-day-rows">
              {items.map((m) => (
                <NoteRow
                  key={m.note_path}
                  item={m}
                  onOpen={onOpen}
                  onDelete={onDeleteRow}
                />
              ))}
            </div>
          </div>
        ))
      )}
    </section>
  );
}

function NoteRow({
  item,
  onOpen,
  onDelete,
}: {
  item: NoteListItem;
  onOpen: (path: string) => void;
  onDelete?: (path: string) => void;
}) {
  const isMeeting = item.duration_ms !== null;
  const [moreOpen, setMoreOpen] = useState(false);

  useEffect(() => {
    if (!moreOpen) return;
    const close = () => setMoreOpen(false);
    window.addEventListener("mousedown", close);
    return () => window.removeEventListener("mousedown", close);
  }, [moreOpen]);

  return (
    <div
      className="home-note-row"
      role="button"
      tabIndex={0}
      onClick={() => onOpen(item.note_path)}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onOpen(item.note_path);
        }
      }}
    >
      <NoteThumb item={item} />
      <div className="home-note-body">
        <div className="home-note-row1">
          <span className="home-note-title">{item.title || "Untitled note"}</span>
          {isMeeting && (
            <span className="home-note-duration">
              <Waveform />
              {formatDuration(item.duration_ms)}
            </span>
          )}
        </div>
        <div className="home-note-preview">
          {item.preview || (isMeeting ? "Recorded meeting" : "Note")}
        </div>
      </div>
      <div className="home-note-meta">
        {item.tags.length > 0 && <TagChips tags={item.tags} max={2} />}
        <span className="home-note-time">{formatTime(item.modified_ms)}</span>
        <div className="nh-popover-anchor home-note-more-anchor">
          <button
            type="button"
            className={"home-note-more" + (moreOpen ? " is-open" : "")}
            aria-label="More"
            title="More"
            onClick={(e) => {
              e.stopPropagation();
              setMoreOpen((v) => !v);
            }}
          >
            <IconMore size={14} sw={1.8} />
          </button>
          {moreOpen && (
            <MoreMenu
              onClose={() => setMoreOpen(false)}
              onDelete={onDelete ? () => onDelete(item.note_path) : undefined}
            />
          )}
        </div>
      </div>
    </div>
  );
}

function NoteThumb({ item }: { item: NoteListItem }) {
  if (item.duration_ms !== null) {
    return (
      <div className="home-note-thumb home-note-thumb-meeting">
        <IconMic size={16} sw={1.7} />
      </div>
    );
  }
  const ch = (item.title || "U").charAt(0).toUpperCase();
  return <div className="home-note-thumb home-note-thumb-note">{ch}</div>;
}

function Waveform() {
  const heights = [3, 6, 4, 8, 5, 7, 3];
  return (
    <svg width="14" height="10" viewBox="0 0 14 10" fill="none" aria-hidden="true">
      {heights.map((h, i) => (
        <rect
          key={i}
          x={i * 2}
          y={(10 - h) / 2}
          width="1.2"
          height={h}
          rx="0.6"
          fill="currentColor"
        />
      ))}
    </svg>
  );
}

// ---------- Ask bar -------------------------------------------------------

function AskBar() {
  return (
    <div className="home-askbar">
      <div className="home-ask">
        <span className="home-ask-icon">
          <IconSparkle size={16} sw={1.6} />
        </span>
        <input
          className="home-ask-input"
          placeholder="Ask Margin anything — summarize a meeting, find a decision, draft an email…"
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              stub("Ask Margin", 29);
            }
          }}
          aria-label="Ask Margin"
        />
        <button
          type="button"
          className="home-ask-send"
          title="Ask — coming soon (issue #29)"
          aria-label="Send"
          onClick={() => stub("Ask Margin", 29)}
        >
          <IconArrowRight size={13} sw={2} />
        </button>
      </div>
    </div>
  );
}

// ---------- Section title -------------------------------------------------

function SectionTitle({
  eyebrow,
  title,
  actionLabel,
  actionIssue,
}: {
  eyebrow: string;
  title: string;
  actionLabel?: string;
  actionIssue?: number;
}) {
  return (
    <div className="home-section-head">
      <div>
        <div className="home-section-eyebrow">{eyebrow}</div>
        <h2 className="home-section-title">{title}</h2>
      </div>
      {actionLabel && (
        <button
          type="button"
          className="home-section-action"
          onClick={() => actionIssue && stub(actionLabel, actionIssue)}
        >
          {actionLabel}
          <IconChevRight size={12} sw={1.9} />
        </button>
      )}
    </div>
  );
}

// ---------- Tag chips -----------------------------------------------------

/// Deterministic chip background tint per tag name. Hash the string and
/// pick from a small palette of muted alpha colors; same tag always ends
/// up the same color across the app.
const TAG_PALETTE = [
  "rgba(196,74,31,0.14)",
  "rgba(58,93,168,0.14)",
  "rgba(79,138,63,0.16)",
  "rgba(110,79,168,0.14)",
  "rgba(168,139,58,0.14)",
  "rgba(58,140,140,0.14)",
];

/// Companion solid-color palette for the Finder-style colored dot used
/// in the sidebar tag list. Same hash → same dot color as chip color.
const TAG_DOT_PALETTE = [
  "#C44A1F",
  "#3A5DA8",
  "#4F8A3F",
  "#6E4FA8",
  "#A88B3A",
  "#3A8C8C",
];

function tagHash(tag: string): number {
  let h = 0;
  for (let i = 0; i < tag.length; i++) h = (h * 31 + tag.charCodeAt(i)) | 0;
  return Math.abs(h);
}

function tagColor(tag: string): string {
  return TAG_PALETTE[tagHash(tag) % TAG_PALETTE.length];
}

function tagDotColor(tag: string): string {
  return TAG_DOT_PALETTE[tagHash(tag) % TAG_DOT_PALETTE.length];
}

function TagChips({ tags, max }: { tags: string[]; max?: number }) {
  if (!tags || tags.length === 0) return null;
  const list = max ? tags.slice(0, max) : tags;
  const overflow = max && tags.length > max ? tags.length - max : 0;
  return (
    <span className="home-tagchips">
      {list.map((t) => (
        <span key={t} className="home-tagchip" style={{ background: tagColor(t) }}>
          {t}
        </span>
      ))}
      {overflow > 0 && <span className="home-tagchip-more">+{overflow}</span>}
    </span>
  );
}

// ---------- helpers -------------------------------------------------------

function dayKey(ms: number): string {
  const d = new Date(ms);
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function groupByDay(items: NoteListItem[]): Map<string, NoteListItem[]> {
  const out = new Map<string, NoteListItem[]>();
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
  const [y, m, d] = key.split("-").map(Number);
  const date = new Date(y, m - 1, d);
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const yesterday = new Date(today);
  yesterday.setDate(today.getDate() - 1);
  if (date.getTime() === today.getTime()) return "Today";
  if (date.getTime() === yesterday.getTime()) return "Yesterday";
  const oneWeekAgo = new Date(today);
  oneWeekAgo.setDate(today.getDate() - 7);
  if (date.getTime() >= oneWeekAgo.getTime()) return "Earlier this week";
  return date.toLocaleDateString(undefined, {
    weekday: "short",
    month: "short",
    day: "numeric",
  });
}

function greetingFor(hour: number): string {
  if (hour < 5) return "Good evening";
  if (hour < 12) return "Good morning";
  if (hour < 18) return "Good afternoon";
  return "Good evening";
}

function inferDisplayName(): string | null {
  // No profile system yet — greet anonymously. When settings grows a
  // displayName field, return it here.
  return null;
}

function avatarColor(initials: string): string {
  const palette = ["#C44A1F", "#3A5DA8", "#4F8A3F", "#6E4FA8", "#A88B3A"];
  let hash = 0;
  for (let i = 0; i < initials.length; i++) hash = (hash * 31 + initials.charCodeAt(i)) | 0;
  return palette[Math.abs(hash) % palette.length];
}

function stub(label: string, issue: number) {
  console.log(`[stub] ${label} clicked — see issue #${issue}`);
}
