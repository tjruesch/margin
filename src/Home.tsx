import { useEffect, useMemo, useRef, useState } from "react";

import { dueBucket, friendlyDueLabel } from "./dueLabel";
import { type ActionListItem, type NoteListItem } from "./file";
import { avatarColor } from "./initials";
import { MoreMenu } from "./MoreMenu";
import { TeamView, type EditorSettings as TeamEditorSettings } from "./Team";
import {
  IconArchive,
  IconBell,
  IconCalendar,
  IconCheck,
  IconChecklist,
  IconChevRight,
  IconHome,
  IconMic,
  IconMore,
  IconPlus,
  IconSearch,
  IconSettings,
  IconSidebar,
  IconStar,
  IconUser,
  IconUsers,
} from "./icons";

type Props = {
  recentFiles: string[];
  notes: NoteListItem[];
  notesLoading: boolean;
  allTags: string[];
  /** "active" → home feed; "archived" → archive view; "favorites" →
   *  favorited-only. App owns this so refreshNotes can fetch the right
   *  scope. */
  scope: "active" | "archived" | "favorites";
  onScopeChange: (next: "active" | "archived" | "favorites") => void;
  onOpen: (path: string) => void;
  onNewNote: () => void;
  onNewMeeting: () => void;
  onOpenSettings: () => void;
  onDeleteRow?: (path: string) => void;
  /** Toggle archived for a row. Caller passes `nextArchived` since the
   *  row only knows the current view's scope, not the per-row state. */
  onArchiveRow?: (path: string, nextArchived: boolean) => void;
  /** Toggle favorite for a row. Caller passes `nextFavorited` based on
   *  the row's per-row `favorite` field. */
  onFavoriteRow?: (path: string, nextFavorited: boolean) => void;
  /** Clone a row to a new bundle. */
  onDuplicateRow?: (path: string) => void;
  /** Open action items across all non-archived owned notes. Drives the
   *  Action items sidebar nav, the count badge, and the home teaser. */
  actions: ActionListItem[];
  /** Flip an action item's done state. Optimistic upstream. */
  onToggleAction: (id: string, nextDone: boolean) => void;
  /** Append a quick todo to the catch-all Inbox note. `dueToken` is the
   *  optional payload after `@` (e.g. `2026-05-15` or `2026-05-15 09:00`);
   *  Rust resolves any relative form during write_note. */
  onAddInboxTodo: (text: string, dueToken: string | null) => Promise<void>;
  /** Editor preferences threaded through so the Team detail view can
   *  hand them to the embedded markdown editor. */
  editor: TeamEditorSettings;
};

type NavId =
  | "home"
  | "actions"
  | "meetings"
  | "shared"
  | "favorites"
  | "archive"
  | "team";
type FilterId = "all" | "notes" | "meetings" | "shared";

function DueChip({ dueMs }: { dueMs: number | null }) {
  if (dueMs == null) return null;
  // Recompute on each render so label flips at midnight when the user
  // returns to the page. Cheap; the home feed already re-renders often.
  const now = Date.now();
  return (
    <span
      className={`home-due ${dueBucket(dueMs, now)}`}
      title={new Date(dueMs).toLocaleString()}
    >
      {friendlyDueLabel(dueMs, now)}
    </span>
  );
}

// Stub data for sections whose backends don't exist yet (#27 calendar).
// Returns empty so the section hides gracefully; swap in live data when
// the backend lands.
const UPCOMING_EVENTS: UpcomingEvent[] = [];

export function Home({
  notes,
  notesLoading,
  allTags,
  scope,
  onScopeChange,
  onOpen,
  onNewNote,
  onNewMeeting,
  onOpenSettings,
  onDeleteRow,
  onArchiveRow,
  onFavoriteRow,
  onDuplicateRow,
  actions,
  onToggleAction,
  onAddInboxTodo,
  editor,
}: Props) {
  const [nav, setNav] = useState<NavId>(
    scope === "archived" ? "archive" : scope === "favorites" ? "favorites" : "home",
  );

  // Map sidebar nav → backend scope. Only home, archive, and favorites
  // currently map to scopes; the other nav items are placeholders.
  useEffect(() => {
    const next: "active" | "archived" | "favorites" =
      nav === "archive" ? "archived" : nav === "favorites" ? "favorites" : "active";
    if (next !== scope) onScopeChange(next);
  }, [nav, scope, onScopeChange]);

  // External "open this nav" requests (e.g. the AttendeePicker's
  // "+ Add team member" link) come in as `margin:nav` events so the
  // sender doesn't have to lift state. Mirrors the dueDatePopover
  // CustomEvent pattern.
  useEffect(() => {
    const VALID: NavId[] = [
      "home",
      "actions",
      "meetings",
      "shared",
      "favorites",
      "archive",
      "team",
    ];
    const onNav = (e: Event) => {
      const detail = (e as CustomEvent<unknown>).detail;
      if (typeof detail === "string" && (VALID as string[]).includes(detail)) {
        setNav(detail as NavId);
      }
    };
    window.addEventListener("margin:nav", onNav);
    return () => window.removeEventListener("margin:nav", onNav);
  }, []);
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

  const openActionCount = actions.filter((a) => !a.done).length;

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

        {nav === "team" ? (
          <TeamView
            editor={editor}
            onOpenNote={onOpen}
            onToggleAction={onToggleAction}
          />
        ) : nav === "actions" ? (
          <ActionsFeed
            actions={actions}
            onToggle={onToggleAction}
            onOpenNote={onOpen}
            onAddInboxTodo={onAddInboxTodo}
          />
        ) : (
          <>
            {openActionCount > 0 && (
              <ActionItemsTeaser
                items={actions}
                onToggle={onToggleAction}
                onOpenNote={onOpen}
                onViewAll={() => setNav("actions")}
              />
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
              onArchiveRow={onArchiveRow}
              onFavoriteRow={onFavoriteRow}
              onDuplicateRow={onDuplicateRow}
              archivedScope={scope === "archived"}
              favoritesScope={scope === "favorites"}
            />
          </>
        )}

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
          icon={<IconUser size={14} sw={1.7} />}
          label="Team"
          active={active === "team"}
          onClick={() => onSelect("team")}
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
        <NavItem
          icon={<IconArchive size={14} sw={1.7} />}
          label="Archive"
          active={active === "archive"}
          onClick={() => onSelect("archive")}
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

function ActionItemsTeaser({
  items,
  onToggle,
  onOpenNote,
  onViewAll,
}: {
  items: ActionListItem[];
  onToggle: (id: string, nextDone: boolean) => void;
  onOpenNote: (path: string) => void;
  onViewAll: () => void;
}) {
  // Show what's currently in state — items are fetched in `open` scope,
  // so anything done here was just ticked off in this page session and
  // we keep it visible (filled checkbox + strikethrough) until the next
  // refresh drops it. The `View all` badge tracks remaining open count.
  const top = items.slice(0, 3);
  const openCount = items.filter((it) => !it.done).length;
  if (top.length === 0) return null;

  return (
    <section className="home-section">
      <div className="home-section-head">
        <div>
          <div className="home-section-eyebrow">Action items</div>
          <h2 className="home-section-title">Things to do</h2>
        </div>
        <button
          type="button"
          className="home-section-action"
          onClick={onViewAll}
          title="See all action items"
        >
          View all ({openCount})
          <IconChevRight size={12} sw={1.7} />
        </button>
      </div>
      <div className="home-actions">
        {top.map((it) => (
          <ActionRow
            key={it.id}
            it={it}
            onToggle={onToggle}
            onOpenNote={onOpenNote}
          />
        ))}
      </div>
    </section>
  );
}

// Order in which due-buckets render. The string keys match the values
// returned by `dueBucket` from dueLabel.ts; the labels are human copy.
export const BUCKET_ORDER = [
  { key: "overdue", label: "Overdue" },
  { key: "today", label: "Today" },
  { key: "soon", label: "This week" },
  { key: "later", label: "Later" },
] as const;

export function ActionRow({
  it,
  onToggle,
  onOpenNote,
}: {
  it: ActionListItem;
  onToggle: (id: string, nextDone: boolean) => void;
  onOpenNote: (path: string) => void;
}) {
  return (
    <div
      className="home-action-row"
      role="button"
      tabIndex={0}
      onClick={() => onOpenNote(it.note_path)}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onOpenNote(it.note_path);
        }
      }}
      title={`Open ${it.note_title}`}
    >
      <button
        type="button"
        className={"home-checkbox" + (it.done ? " done" : "")}
        aria-label={it.done ? "Mark as open" : "Mark as done"}
        onClick={(e) => {
          // Don't navigate when the user is just toggling done.
          e.stopPropagation();
          onToggle(it.id, !it.done);
        }}
      >
        {it.done && <IconCheck size={20} sw={3.6} />}
      </button>
      <div className="home-action-body">
        <div className={"home-action-text" + (it.done ? " done" : "")}>{it.text}</div>
      </div>
      <DueChip dueMs={it.due_ms} />
    </div>
  );
}

// ---- Inbox composer -----------------------------------------------------

function InboxComposer({
  onAdd,
}: {
  onAdd: (text: string, dueToken: string | null) => Promise<void>;
}) {
  const [open, setOpen] = useState(false);
  const [text, setText] = useState("");
  const [dateStr, setDateStr] = useState("");
  const [includeTime, setIncludeTime] = useState(false);
  const [timeStr, setTimeStr] = useState("09:00");
  const [busy, setBusy] = useState(false);
  const inputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    if (open) inputRef.current?.focus();
  }, [open]);

  const reset = () => {
    setText("");
    setDateStr("");
    setIncludeTime(false);
    setTimeStr("09:00");
  };

  const submit = async () => {
    const trimmed = text.trim();
    if (!trimmed || busy) return;
    const dueToken = dateStr
      ? includeTime
        ? `${dateStr} ${timeStr}`
        : dateStr
      : null;
    setBusy(true);
    try {
      await onAdd(trimmed, dueToken);
      reset();
    } finally {
      setBusy(false);
    }
  };

  if (!open) {
    return (
      <button
        type="button"
        className="inbox-composer-toggle"
        onClick={() => setOpen(true)}
      >
        <IconPlus size={12} sw={1.8} />
        New todo
      </button>
    );
  }

  return (
    <div className="inbox-composer">
      <input
        ref={inputRef}
        type="text"
        className="inbox-composer-text"
        placeholder="What needs doing?"
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            void submit();
          } else if (e.key === "Escape") {
            setOpen(false);
            reset();
          }
        }}
      />
      <div className="inbox-composer-actions">
        <input
          type="date"
          className="inbox-composer-date"
          lang="de-DE"
          value={dateStr}
          onChange={(e) => setDateStr(e.target.value)}
        />
        <label className="inbox-composer-timetoggle">
          <input
            type="checkbox"
            checked={includeTime}
            onChange={(e) => setIncludeTime(e.target.checked)}
          />
          Time
        </label>
        {includeTime && (
          <input
            type="time"
            className="inbox-composer-time"
            lang="de-DE"
            value={timeStr}
            onChange={(e) => setTimeStr(e.target.value)}
          />
        )}
        <div className="inbox-composer-spacer" />
        <button
          type="button"
          className="inbox-composer-cancel"
          onClick={() => {
            setOpen(false);
            reset();
          }}
        >
          Cancel
        </button>
        <button
          type="button"
          className="inbox-composer-save"
          disabled={!text.trim() || busy}
          onClick={() => void submit()}
        >
          {busy ? "Adding…" : "Add"}
        </button>
      </div>
    </div>
  );
}

function ActionsFeed({
  actions,
  onToggle,
  onOpenNote,
  onAddInboxTodo,
}: {
  actions: ActionListItem[];
  onToggle: (id: string, nextDone: boolean) => void;
  onOpenNote: (path: string) => void;
  onAddInboxTodo: (text: string, dueToken: string | null) => Promise<void>;
}) {
  // Split dated vs. undated, then bucket the dated half by urgency.
  // Backend already orders dated rows by `due_ms ASC`, so each bucket is
  // chronological without further sorting. Undated rows fall into one
  // flat catch-all bucket at the bottom.
  const { byBucket, undated } = useMemo(() => {
    const now = Date.now();
    const buckets: Record<string, ActionListItem[]> = {
      overdue: [],
      today: [],
      soon: [],
      later: [],
    };
    const undatedRows: ActionListItem[] = [];
    for (const a of actions) {
      if (a.due_ms != null) {
        buckets[dueBucket(a.due_ms, now)].push(a);
      } else {
        undatedRows.push(a);
      }
    }
    return { byBucket: buckets, undated: undatedRows };
  }, [actions]);

  if (actions.length === 0) {
    return (
      <section className="home-section">
        <div className="home-section-head">
          <div>
            <div className="home-section-eyebrow">Action items</div>
            <h2 className="home-section-title">Things to do</h2>
          </div>
        </div>
        <InboxComposer onAdd={onAddInboxTodo} />
        <p className="home-empty">
          No open action items. Add <code>- [ ] task</code> lines to any note,
          trail with <code>@2026-01-15</code> / <code>@tomorrow</code> /{" "}
          <code>@friday</code> to schedule, or hit <em>New todo</em> above.
        </p>
      </section>
    );
  }

  return (
    <section className="home-section">
      <div className="home-section-head">
        <div>
          <div className="home-section-eyebrow">Action items</div>
          <h2 className="home-section-title">Things to do</h2>
        </div>
      </div>

      <InboxComposer onAdd={onAddInboxTodo} />

      {BUCKET_ORDER.map(({ key, label }) => {
        const items = byBucket[key];
        if (items.length === 0) return null;
        return (
          <div key={key} className={`home-action-bucket bucket-${key}`}>
            <div className="home-action-bucket-head">
              <span className="home-action-bucket-label">{label}</span>
              <span className="home-action-bucket-count">{items.length}</span>
            </div>
            <div className="home-actions">
              {items.map((it) => (
                <ActionRow
                  key={it.id}
                  it={it}
                  onToggle={onToggle}
                  onOpenNote={onOpenNote}
                />
              ))}
            </div>
          </div>
        );
      })}

      {undated.length > 0 && (
        <div className="home-action-bucket bucket-undated">
          <div className="home-action-bucket-head">
            <span className="home-action-bucket-label">No due date</span>
            <span className="home-action-bucket-count">{undated.length}</span>
          </div>
          <div className="home-actions">
            {undated.map((it) => (
              <ActionRow
                key={it.id}
                it={it}
                onToggle={onToggle}
                onOpenNote={onOpenNote}
              />
            ))}
          </div>
        </div>
      )}
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
  onArchiveRow,
  onFavoriteRow,
  onDuplicateRow,
  archivedScope,
  favoritesScope,
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
  onArchiveRow?: (path: string, nextArchived: boolean) => void;
  onFavoriteRow?: (path: string, nextFavorited: boolean) => void;
  onDuplicateRow?: (path: string) => void;
  archivedScope: boolean;
  favoritesScope: boolean;
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
          {archivedScope
            ? "No archived notes yet."
            : favoritesScope
              ? "No favorites yet — star a note from its More menu to pin it here."
              : (
                <>
                  No notes yet — press <kbd>⌘N</kbd> for a new note,{" "}
                  <kbd>⌘⇧M</kbd> to start one with a recording.
                </>
              )}
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
                  onArchive={onArchiveRow}
                  archived={archivedScope}
                  onFavorite={onFavoriteRow}
                  onDuplicate={onDuplicateRow}
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
  onArchive,
  archived,
  onFavorite,
  onDuplicate,
}: {
  item: NoteListItem;
  onOpen: (path: string) => void;
  onDelete?: (path: string) => void;
  onArchive?: (path: string, nextArchived: boolean) => void;
  /** Whether the row's note is archived. Determined by the feed's scope:
   *  in active view it's always false, in archive view always true. */
  archived?: boolean;
  onFavorite?: (path: string, nextFavorited: boolean) => void;
  onDuplicate?: (path: string) => void;
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
        {item.favorite && (
          <span className="home-note-fav-star" title="Favorite" aria-label="Favorite">
            <IconStar size={12} sw={1.7} />
          </span>
        )}
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
              onArchive={
                onArchive
                  ? () => onArchive(item.note_path, !archived)
                  : undefined
              }
              archived={archived}
              onFavorite={
                onFavorite
                  ? () => onFavorite(item.note_path, !item.favorite)
                  : undefined
              }
              favorited={item.favorite}
              onDuplicate={
                onDuplicate ? () => onDuplicate(item.note_path) : undefined
              }
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

function stub(label: string, issue: number) {
  console.log(`[stub] ${label} clicked — see issue #${issue}`);
}
