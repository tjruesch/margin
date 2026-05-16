import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { AssigneeChip, stripLeadingOwnerPrefix } from "./AssigneeChip";
import { dueBucket, friendlyDueLabel } from "./dueLabel";
import {
  type ActionListItem,
  type CalendarEvent,
  type ConnectorStatusEvent,
  deleteOpenQuestion,
  listCalendarEvents,
  listOpenQuestions,
  type NoteListItem,
  type OpenQuestionItem,
  openOrCreateEventNote,
  type QuestionScope,
  reopenOpenQuestion,
  resolveOpenQuestion,
  setOpenQuestionAskedOf,
  type TeamMember,
  type Workstream,
} from "./file";
import { ResolveQuestionPopover } from "./ResolveQuestionPopover";
import { WorkstreamChip } from "./WorkstreamChip";
import { ActivityPanel } from "./ActivityPanel";
import { avatarColor } from "./initials";
import { MoreMenu } from "./MoreMenu";
import { type NotificationRecord, unreadCount } from "./notifications";
import { NotificationsPanel } from "./NotificationsPanel";
import { SearchPalette } from "./SearchPalette";
import { TeamView } from "./Team";
import { WorkstreamsView } from "./Workstreams";
import {
  IconArchive,
  IconBell,
  IconBriefcase,
  IconCalendar,
  IconChat,
  IconCheck,
  IconChecklist,
  IconChevRight,
  IconFileText,
  IconHelp,
  IconHome,
  IconMail,
  IconMic,
  IconMore,
  IconPlus,
  IconSearch,
  IconSettings,
  IconSidebar,
  IconSparkle,
  IconStar,
  IconTrash,
  IconUser,
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
  /** Currently-unresolved open questions count for the sidebar badge
   *  (#113). Updated alongside `actions` on every save. */
  openQuestionCount: number;
  /** Flip an action item's done state. Optimistic upstream. */
  onToggleAction: (id: string, nextDone: boolean) => void;
  /** Permanently remove an action item's checkbox line from its source
   *  note (#100). Optimistic upstream. */
  onDeleteAction: (id: string) => void;
  /** Append a quick todo to the catch-all Inbox note. `dueToken` is the
   *  optional payload after `@` (e.g. `2026-05-15` or `2026-05-15 09:00`);
   *  Rust resolves any relative form during write_note. */
  onAddInboxTodo: (text: string, dueToken: string | null) => Promise<void>;
  /** Team members for the assignee-chip dropdown on action rows (#51). */
  members: TeamMember[];
  /** Reassign an action to a different team member (or null to unassign).
   *  Body-rewrites the source line; the upstream refetch picks up the
   *  new assignee_id via the resolver. */
  onReassignAction: (actionId: string, memberId: string | null) => Promise<void>;
  /** Pin (or unpin with null) an action to a workstream (#111). */
  onReattachActionWorkstream: (
    actionId: string,
    workstreamId: string | null,
  ) => Promise<void>;
  /** Reopen + lock a worker-auto-resolved action (#124). Optimistic
   *  upstream — the upstream refetch sweeps `auto_resolved_ms = null`. */
  onUndoAutoResolved: (id: string) => Promise<void>;
  /** In-app notification queue surfaced by the bell button (#37). */
  notifications: NotificationRecord[];
  /** Stamp `read_at` on every unread notification. Called when the
   *  user opens the panel. */
  onMarkAllNotificationsRead: () => void;
  /** Synthesized workstreams (#71). Cards in the Workstreams view. */
  workstreams: import("./file").Workstream[];
  workstreamsLoading: boolean;
  synthInFlight: boolean;
  synthMessage: string | null;
  /** Force a synthesis pass via `synthesize_workstreams(true)`. */
  onRefreshWorkstreams: () => void;
  /** Callback fired by WorkstreamsView whenever a workstream is
   *  created, archived, snoozed, reparented, or has its owner changed.
   *  App.tsx refetches the list so the list view reflects the change
   *  without a synth pass. (#101) */
  onWorkstreamsChanged: () => void;
};

type NavId =
  | "home"
  | "actions"
  | "openquestions"
  | "workstreams"
  | "favorites"
  | "archive"
  | "team";
type FilterId = "all" | "notes" | "meetings";

export function DueChip({ dueMs }: { dueMs: number | null }) {
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

/// Live calendar data hook (#62). Returns mapped `UpcomingEvent`s for
/// the next 24 hours plus a 30-minute look-back window so an
/// in-progress meeting still appears with a "Now" indicator. Refetches
/// on every `connector-status` Tauri event so a freshly-synced
/// connector lights up immediately. Ticks once per minute so relative
/// labels ("in 12 min" → "in 11 min") update without manual refresh.
function useUpcomingEvents(): {
  upcoming: UpcomingEvent[];
  raw: CalendarEvent[];
  nowMs: number;
} {
  const [events, setEvents] = useState<CalendarEvent[]>([]);
  const [tickMs, setTickMs] = useState<number>(() => Date.now());

  useEffect(() => {
    let cancelled = false;
    const load = async () => {
      const now = Date.now();
      try {
        const list = await listCalendarEvents(
          now - 30 * 60_000,
          now + 48 * 3600_000,
        );
        if (!cancelled) setEvents(list);
      } catch (e) {
        if (!cancelled) console.error("[home] listCalendarEvents failed:", e);
      }
    };
    void load();
    let unlisten: UnlistenFn | null = null;
    (async () => {
      const fn = await listen<ConnectorStatusEvent>("connector-status", () => {
        void load();
      });
      if (cancelled) {
        fn();
      } else {
        unlisten = fn;
      }
    })();
    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, []);

  useEffect(() => {
    const id = window.setInterval(() => setTickMs(Date.now()), 60_000);
    return () => window.clearInterval(id);
  }, []);

  const upcoming = useMemo(
    () =>
      events
        .filter(
          (e) =>
            // Defensive against any event whose start_ms didn't parse —
            // would render with a misleading "Now" label.
            e.start_ms > tickMs - 24 * 3600_000 && e.status !== "cancelled",
        )
        // Cap at 5 cards. Events arrive sorted by start_ms, so this
        // keeps the soonest 5 — anything further out is past what the
        // strip is meant to surface.
        .slice(0, 3)
        .map((e) => mapToUpcoming(e, tickMs)),
    [events, tickMs],
  );

  return { upcoming, raw: events, nowMs: tickMs };
}

function mapToUpcoming(e: CalendarEvent, nowMs: number): UpcomingEvent {
  const live = e.start_ms <= nowMs && nowMs < e.end_ms;
  return {
    id: e.id,
    when: live ? "now" : formatRelative(e.start_ms, nowMs),
    tStart: formatHm(e.start_ms),
    tEnd: formatHm(e.end_ms),
    title: e.title,
    attendees: e.attendees
      .filter((a) => !a.is_self)
      .slice(0, 8)
      .map((a) => initialsOf(a.display_name ?? a.email)),
    tags: [],
    color: connectorColor(e.connector_id),
    live,
  };
}

function formatHm(ms: number): string {
  return new Intl.DateTimeFormat([], {
    hour: "numeric",
    minute: "2-digit",
  }).format(new Date(ms));
}

/// Past-only relative time for action-row sublines (#123). The
/// existing `formatRelative` is future-focused (due dates); this one
/// clamps to >= 0 so old timestamps render as "2d ago" rather than
/// "in -2 days".
function timeAgo(ms: number, nowMs: number): string {
  const dt = Math.max(0, nowMs - ms);
  const m = Math.floor(dt / 60_000);
  if (m < 1) return "just now";
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.floor(h / 24);
  if (d < 30) return `${d}d ago`;
  const mo = Math.floor(d / 30);
  if (mo < 12) return `${mo}mo ago`;
  return `${Math.floor(mo / 12)}y ago`;
}

function formatRelative(startMs: number, nowMs: number): string {
  const delta = startMs - nowMs;
  if (delta < 60_000) return "now";
  const min = Math.floor(delta / 60_000);
  if (min < 60) return `in ${min} min`;

  const startDay = startOfDay(startMs);
  const todayDay = startOfDay(nowMs);
  if (startDay === todayDay) {
    const h = Math.floor(min / 60);
    const m = min % 60;
    return m > 0 ? `in ${h}h ${m}m` : `in ${h}h`;
  }
  if (startDay === todayDay + 24 * 3600_000) {
    return `tomorrow at ${formatHm(startMs)}`;
  }
  const weekday = new Intl.DateTimeFormat([], { weekday: "short" }).format(
    new Date(startMs),
  );
  return `${weekday} at ${formatHm(startMs)}`;
}

function startOfDay(ms: number): number {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/// Two-character initials extracted from a display name or email. For
/// emails we use the local part. AttendeeStack already calls
/// `avatarColor()` per initials string so different attendees get
/// different chip colors.
function initialsOf(s: string): string {
  const trimmed = s.trim();
  if (trimmed.length === 0) return "?";
  // Email: take the first letter of the local part.
  if (trimmed.includes("@")) {
    const local = trimmed.split("@")[0];
    return (local.slice(0, 2) || "?").toUpperCase();
  }
  // Name: first letter of first two whitespace-separated tokens.
  const parts = trimmed.split(/\s+/).filter(Boolean);
  if (parts.length === 1) return parts[0].slice(0, 2).toUpperCase();
  return (parts[0][0] + parts[1][0]).toUpperCase();
}

/// Map connector_id to a stable accent color. Provider-aware lookup —
/// Microsoft = MS blue, Google = Google blue, others = neutral. v1
/// keeps this static; per-account variation can be a v2 nicety.
function connectorColor(connectorId: string): string {
  const kind = connectorId.split(":")[0];
  switch (kind) {
    case "microsoft_graph":
      return "#0078d4";
    case "google_calendar":
      return "#1a73e8";
    default:
      return "#888";
  }
}

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
  openQuestionCount,
  onToggleAction,
  onDeleteAction,
  onAddInboxTodo,
  members,
  onReassignAction,
  onReattachActionWorkstream,
  onUndoAutoResolved,
  notifications,
  onMarkAllNotificationsRead,
  workstreams,
  workstreamsLoading,
  synthInFlight,
  synthMessage,
  onRefreshWorkstreams,
  onWorkstreamsChanged,
}: Props) {
  const [nav, setNav] = useState<NavId>(
    scope === "archived" ? "archive" : scope === "favorites" ? "favorites" : "home",
  );
  const [panelOpen, setPanelOpen] = useState(false);
  const [activityOpen, setActivityOpen] = useState(false);
  const [paletteOpen, setPaletteOpen] = useState(false);
  // Live count of unassigned items for the Workstreams page pill (#108).
  // Refreshed on workstreams-view enter and whenever the modal fires
  // `margin:unassigned-changed` (attach / detach moves an item across
  // the count). One IPC per refresh — list_unassigned_items returns
  // the rows; we just take .length.
  const [unassignedCount, setUnassignedCount] = useState(0);
  useEffect(() => {
    let cancelled = false;
    const refresh = () => {
      void import("./file").then(({ listUnassignedItems }) =>
        listUnassignedItems().then((items) => {
          if (!cancelled) setUnassignedCount(items.length);
        }),
      );
    };
    if (nav === "workstreams") refresh();
    window.addEventListener("margin:unassigned-changed", refresh);
    return () => {
      cancelled = true;
      window.removeEventListener("margin:unassigned-changed", refresh);
    };
  }, [nav]);
  // Counter-style tracking of open detail breadcrumbs. Each
  // `PageHeaderBreadcrumb` mount increments, unmount decrements.
  // Greater than 0 means a detail view is active and the list-scoped
  // page actions ("+ Add team member" etc.) should be hidden.
  const [detailOpenCount, setDetailOpenCount] = useState(0);
  useEffect(() => {
    const onOpen = () => setDetailOpenCount((n) => n + 1);
    const onClose = () => setDetailOpenCount((n) => Math.max(0, n - 1));
    window.addEventListener("margin:page-detail-open", onOpen);
    window.addEventListener("margin:page-detail-closed", onClose);
    return () => {
      window.removeEventListener("margin:page-detail-open", onOpen);
      window.removeEventListener("margin:page-detail-closed", onClose);
    };
  }, []);
  const unreadBadge = useMemo(() => unreadCount(notifications), [notifications]);

  // Global keyboard shortcuts for the search palette:
  //   ⌘K          — open palette in lexical search mode
  //   Space hold  — open palette in voice mode; release to transcribe.
  //                 Only fires when no editable element is focused, so
  //                 typing in inputs / note editor still gets a space.
  // Capture phase so the editor's Cmd+K (link insert, etc.) doesn't
  // swallow it first.
  useEffect(() => {
    let voiceArmed = false;

    // True when the active element is something the user is likely
    // typing into. Skips the space-hold trigger so normal typing isn't
    // hijacked. CodeMirror's content area is contenteditable=true so
    // it gets caught by the isContentEditable check.
    const isEditable = (el: Element | null) => {
      if (!el) return false;
      const tag = el.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return true;
      return (el as HTMLElement).isContentEditable === true;
    };

    const onDown = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      // ⌘K — existing palette open in search mode.
      if (mod && (e.key === "k" || e.key === "K")) {
        e.preventDefault();
        e.stopPropagation();
        setPaletteOpen(true);
        return;
      }
      // Space hold — voice mode. Skip if any modifier is held (so
      // ⌘+Space, ⌃+Space etc. continue to work for OS shortcuts) or
      // if the user is currently typing.
      if (
        e.code === "Space" &&
        !e.metaKey &&
        !e.ctrlKey &&
        !e.altKey &&
        !e.shiftKey
      ) {
        if (isEditable(document.activeElement)) return;
        e.preventDefault();
        e.stopPropagation();
        if (voiceArmed || e.repeat) return; // ignore autorepeat
        voiceArmed = true;
        setPaletteOpen(true);
        window.dispatchEvent(new CustomEvent("margin:voice-start"));
      }
    };

    const onUp = (e: KeyboardEvent) => {
      if (!voiceArmed) return;
      if (e.code === "Space") {
        voiceArmed = false;
        window.dispatchEvent(new CustomEvent("margin:voice-stop"));
      }
    };

    window.addEventListener("keydown", onDown, { capture: true });
    window.addEventListener("keyup", onUp, { capture: true });
    return () => {
      window.removeEventListener("keydown", onDown, { capture: true } as EventListenerOptions);
      window.removeEventListener("keyup", onUp, { capture: true } as EventListenerOptions);
    };
  }, []);

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
      "workstreams",
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
      default:
        return list;
    }
  }, [notes, filter, tagFilter]);

  const grouped = useMemo(() => groupByDay(filteredNotes), [filteredNotes]);

  const openActionCount = actions.filter((a) => !a.done).length;

  const { upcoming, raw: rawEvents, nowMs: eventsNowMs } = useUpcomingEvents();

  // Today-bound count for the greeting subline. Counts events whose
  // start falls between today's midnight and tomorrow's, ignoring
  // cancelled events.
  const todayCount = useMemo(() => {
    const dayStart = startOfDay(eventsNowMs);
    const dayEnd = dayStart + 24 * 3600_000;
    return rawEvents.filter(
      (e) =>
        e.status !== "cancelled" &&
        e.start_ms >= dayStart &&
        e.start_ms < dayEnd,
    ).length;
  }, [rawEvents, eventsNowMs]);

  // The next future event drives the greeting accent ("1 starts in
  // Xm"). Skip in-progress events here — those already get a Now
  // pulse on their card.
  const nextEvent = useMemo(
    () => upcoming.find((e) => !e.live) ?? null,
    [upcoming],
  );

  const openEventNote = async (eventId: string) => {
    try {
      const path = await openOrCreateEventNote(eventId);
      onOpen(path);
    } catch (e) {
      console.error("[home] openOrCreateEventNote failed:", e);
    }
  };

  // Switch to the Workstreams view and surface the detail for `id`
  // (#100). Mirrors the SearchPalette's `onOpenWorkstream` path —
  // setNav then dispatch a microtask later so WorkstreamsView is
  // mounted in time to receive the event.
  const openWorkstream = (id: string) => {
    setNav("workstreams");
    queueMicrotask(() => {
      window.dispatchEvent(
        new CustomEvent("margin:open-workstream", { detail: id }),
      );
    });
  };

  return (
    <div className={"home" + (sidebarOpen ? "" : " home-collapsed")}>
      <SearchPalette
        open={paletteOpen}
        onClose={() => setPaletteOpen(false)}
        onOpenNote={(p) => {
          setPaletteOpen(false);
          onOpen(p);
        }}
        onOpenWorkstream={(id) => {
          setPaletteOpen(false);
          setNav("workstreams");
          // setNav is synchronous setState; the WorkstreamsView mounts
          // on the next render. Defer the detail-open dispatch to the
          // following microtask so the View's listener is wired up by
          // the time the event fires.
          queueMicrotask(() => {
            window.dispatchEvent(
              new CustomEvent("margin:open-workstream", { detail: id }),
            );
          });
        }}
      />
      {sidebarOpen && (
        <Sidebar
          active={nav}
          onSelect={setNav}
          actionCount={openActionCount}
          openQuestionCount={openQuestionCount}
          workstreamCount={workstreams.filter((w) => w.open_action_count > 0).length}
          tags={allTags}
          activeTag={tagFilter}
          onTagSelect={(t) => setTagFilter(t === tagFilter ? null : t)}
          onOpenSettings={onOpenSettings}
          onOpenPalette={() => setPaletteOpen(true)}
          onNewNote={onNewNote}
          onNewMeeting={onNewMeeting}
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
          <div className="notifications-anchor">
            <button
              type="button"
              className={"home-icon-btn" + (activityOpen ? " active" : "")}
              title="Today's activity"
              aria-label="Today's activity"
              onClick={(e) => {
                e.stopPropagation();
                const opening = !activityOpen;
                setActivityOpen(opening);
                if (opening) setPanelOpen(false);
              }}
            >
              <IconSparkle size={14} sw={1.6} />
            </button>
            <ActivityPanel
              open={activityOpen}
              onClose={() => setActivityOpen(false)}
              onOpenObservation={(memberId, highlightObsId) => {
                // Switch to the Team view first (#116). The
                // `margin:open-team-member` event is dispatched on
                // the next tick so TeamView's listener is mounted by
                // the time it fires.
                window.dispatchEvent(
                  new CustomEvent("margin:nav", { detail: "team" }),
                );
                setTimeout(() => {
                  window.dispatchEvent(
                    new CustomEvent("margin:open-team-member", {
                      detail: { memberId, highlightObsId },
                    }),
                  );
                }, 0);
              }}
            />
          </div>
          <div className="notifications-anchor">
            <button
              type="button"
              className={"home-icon-btn" + (panelOpen ? " active" : "")}
              title="Notifications"
              aria-label="Notifications"
              onClick={(e) => {
                e.stopPropagation();
                const opening = !panelOpen;
                setPanelOpen(opening);
                if (opening) {
                  setActivityOpen(false);
                  onMarkAllNotificationsRead();
                }
              }}
            >
              <IconBell size={14} sw={1.6} />
              {unreadBadge > 0 && <span className="home-icon-btn-dot" />}
            </button>
            <NotificationsPanel
              open={panelOpen}
              notifications={notifications}
              onClose={() => setPanelOpen(false)}
              onOpenNote={(p) => {
                setPanelOpen(false);
                onOpen(p);
              }}
            />
          </div>
        </div>

        {nav === "home" ? (
          <Greeting
            upcomingCount={todayCount}
            nextEvent={nextEvent}
            onNewNote={onNewNote}
            onNewMeeting={onNewMeeting}
          />
        ) : (
          detailOpenCount > 0 ? null : (
            <PageHeader
              title={pageHeaderTitle(nav)}
              actions={renderScopedActions(nav, {
                onRefreshWorkstreams,
                synthInFlight,
                unassignedCount,
              })}
            />
          )
        )}

        {nav === "home" && upcoming.length > 0 && (
          <UpcomingStrip events={upcoming} onOpen={openEventNote} />
        )}

        {nav === "team" ? (
          <TeamView
            onOpenNote={onOpen}
            onOpenWorkstream={openWorkstream}
            onToggleAction={onToggleAction}
            onReassignAction={onReassignAction}
          />
        ) : nav === "workstreams" ? (
          <WorkstreamsView
            workstreams={workstreams}
            loading={workstreamsLoading}
            synthInFlight={synthInFlight}
            synthMessage={synthMessage}
            onOpenNote={onOpen}
            onChanged={onWorkstreamsChanged}
          />
        ) : nav === "actions" ? (
          <ActionsFeed
            actions={actions}
            onToggle={onToggleAction}
            onDelete={onDeleteAction}
            onOpenNote={onOpen}
            onOpenWorkstream={openWorkstream}
            onAddInboxTodo={onAddInboxTodo}
            members={members}
            workstreams={workstreams}
            onReassign={onReassignAction}
            onReattachWorkstream={onReattachActionWorkstream}
            onUndoAutoResolved={(id) => {
              void onUndoAutoResolved(id);
            }}
          />
        ) : nav === "openquestions" ? (
          <OpenQuestionsFeed
            onOpenNote={onOpen}
            members={members}
            workstreams={workstreams}
          />
        ) : (
          <>
            {nav !== "favorites" && openActionCount > 0 && (
              <ActionItemsTeaser
                items={actions}
                onToggle={onToggleAction}
                onOpenNote={onOpen}
                onOpenWorkstream={openWorkstream}
                onViewAll={() => setNav("actions")}
                members={members}
                onReassign={onReassignAction}
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
  openQuestionCount,
  workstreamCount,
  tags,
  activeTag,
  onTagSelect,
  onOpenSettings,
  onOpenPalette,
  onNewNote,
  onNewMeeting,
}: {
  active: NavId;
  onSelect: (id: NavId) => void;
  actionCount: number;
  openQuestionCount: number;
  workstreamCount: number;
  tags: string[];
  activeTag: string | null;
  onTagSelect: (tag: string) => void;
  onOpenSettings: () => void;
  onOpenPalette: () => void;
  /** Global compose CTAs. The Greeting on Home renders these inline;
   *  on every other nav we surface them from the sidebar footer
   *  instead so they stay reachable without crowding the page header
   *  (where scoped page actions live). */
  onNewNote: () => void;
  onNewMeeting: () => void;
}) {
  return (
    <aside className="home-sidebar">
      <div className="home-titlebar" data-tauri-drag-region />
      <div className="home-search-wrap">
        <button
          type="button"
          className="home-search"
          data-tauri-drag-region="false"
          title="Search notes (⌘K)"
          aria-label="Search notes"
          onClick={onOpenPalette}
        >
          <IconSearch size={13} sw={1.8} />
          <span className="home-search-placeholder">Search notes…</span>
          <span className="home-search-kbd">⌘K</span>
        </button>
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
          icon={<IconHelp size={14} sw={1.7} />}
          label="Open questions"
          badge={openQuestionCount > 0 ? String(openQuestionCount) : null}
          active={active === "openquestions"}
          onClick={() => onSelect("openquestions")}
        />
        <NavItem
          icon={<IconBriefcase size={14} sw={1.7} />}
          label="Workstreams"
          badge={workstreamCount > 0 ? String(workstreamCount) : null}
          active={active === "workstreams"}
          onClick={() => onSelect("workstreams")}
        />
        <NavItem
          icon={<IconUser size={14} sw={1.7} />}
          label="Team"
          active={active === "team"}
          onClick={() => onSelect("team")}
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
        {active !== "home" ? (
          <div className="home-side-cta">
            <button
              type="button"
              className="home-side-cta-secondary"
              onClick={onNewNote}
            >
              <IconPlus size={12} sw={1.8} />
              New note
            </button>
            <button
              type="button"
              className="home-side-cta-primary"
              onClick={onNewMeeting}
            >
              <span className="home-cta-dot" />
              New meeting
            </button>
          </div>
        ) : null}
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

/** Page-scoped actions for the right side of `PageHeader`. Composer
 *  toggles dispatch CustomEvents because the composer state lives
 *  inside the sub-page component (TeamSection / ActionsFeed); the
 *  page header just fires the open trigger. Refresh on Workstreams
 *  has its handler already at Home.tsx scope so it's a direct call. */
function renderScopedActions(
  nav: NavId,
  ctx: {
    onRefreshWorkstreams: () => void;
    synthInFlight: boolean;
    unassignedCount: number;
  },
): React.ReactNode {
  switch (nav) {
    case "team":
      return (
        <button
          type="button"
          className="home-section-add"
          onClick={() =>
            window.dispatchEvent(new CustomEvent("margin:open-team-composer"))
          }
        >
          <IconPlus size={13} sw={1.8} />
          Add team member
        </button>
      );
    case "actions":
      return (
        <button
          type="button"
          className="home-section-add"
          onClick={() =>
            window.dispatchEvent(new CustomEvent("margin:open-actions-composer"))
          }
        >
          <IconPlus size={13} sw={1.8} />
          New todo
        </button>
      );
    case "workstreams":
      return (
        <>
          <button
            type="button"
            className="home-section-add"
            onClick={() =>
              window.dispatchEvent(
                new CustomEvent("margin:open-unassigned"),
              )
            }
            title="Recent entities not attached to any workstream"
          >
            Unassigned ({ctx.unassignedCount})
          </button>
          <button
            type="button"
            className="home-section-add"
            onClick={() =>
              window.dispatchEvent(
                new CustomEvent("margin:open-workstream-composer"),
              )
            }
          >
            <IconPlus size={13} sw={1.8} />
            New workstream
          </button>
          <button
            type="button"
            className="home-section-add"
            onClick={ctx.onRefreshWorkstreams}
            disabled={ctx.synthInFlight}
          >
            {ctx.synthInFlight ? "Synthesizing…" : "Refresh"}
          </button>
        </>
      );
    case "favorites":
    case "archive":
    case "home":
      return null;
  }
}

function pageHeaderTitle(nav: NavId): string {
  switch (nav) {
    case "actions":
      return "Action items";
    case "openquestions":
      return "Open questions";
    case "workstreams":
      return "Workstreams";
    case "team":
      return "Team";
    case "favorites":
      return "Favorites";
    case "archive":
      return "Archive";
    case "home":
      // Home renders the Greeting variant; this branch is unreachable
      // from the call site but kept exhaustive for the compiler.
      return "Margin";
  }
}

function PageHeader({
  title,
  actions,
}: {
  title: string;
  /** Page-scoped actions rendered on the right of the header. e.g.
   *  "Add team member" on Team, "New todo" on Actions, "Refresh" on
   *  Workstreams. The global New-note / New-meeting CTAs live in the
   *  sidebar footer for non-home pages instead of competing here. */
  actions?: React.ReactNode;
}) {
  return (
    <header className="home-greeting page-header">
      <div className="home-greeting-text">
        <h1 className="home-greeting-title">{title}</h1>
      </div>
      {actions ? <div className="home-greeting-cta">{actions}</div> : null}
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

function UpcomingStrip({
  events,
  onOpen,
}: {
  events: UpcomingEvent[];
  onOpen: (eventId: string) => void;
}) {
  return (
    <section className="home-section">
      <SectionTitle eyebrow="Upcoming" title="Coming up" />
      <div className="home-upcoming">
        {events.map((ev) => (
          <button
            key={ev.id}
            type="button"
            className="home-upcoming-card"
            onClick={() => onOpen(ev.id)}
          >
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
  onOpenWorkstream,
  onViewAll,
  members,
  onReassign,
}: {
  items: ActionListItem[];
  onToggle: (id: string, nextDone: boolean) => void;
  onOpenNote: (path: string) => void;
  onOpenWorkstream: (id: string) => void;
  onViewAll: () => void;
  members: TeamMember[];
  onReassign: (actionId: string, memberId: string | null) => void;
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
            onOpenWorkstream={onOpenWorkstream}
            members={members}
            onReassign={onReassign}
          />
        ))}
      </div>
    </section>
  );
}

function ActionsFilterBar({
  filter,
  onChange,
  members,
  workstreams,
  selfId,
  counts,
}: {
  filter: ActionsFilter;
  onChange: (next: ActionsFilter) => void;
  members: TeamMember[];
  workstreams: Workstream[];
  selfId: string | null;
  counts: { all: number; mine: number; unassigned: number };
}) {
  // Members excluding self for the per-person picker. Sorted by display
  // name to keep the dropdown predictable across renders.
  const others = useMemo(
    () =>
      [...members]
        .filter((m) => m.id !== selfId)
        .sort((a, b) => a.display_name.localeCompare(b.display_name)),
    [members, selfId],
  );

  // Active workstreams alphabetized for the per-workstream picker.
  const sortedWorkstreams = useMemo(
    () =>
      [...workstreams].sort((a, b) =>
        a.title.localeCompare(b.title, undefined, { sensitivity: "base" }),
      ),
    [workstreams],
  );

  const memberSelected =
    filter.kind === "member"
      ? others.find((m) => m.id === filter.memberId)
      : undefined;
  const workstreamSelected =
    filter.kind === "workstream"
      ? sortedWorkstreams.find((w) => w.id === filter.workstreamId)
      : undefined;

  return (
    <div className="actions-filter-bar">
      <button
        type="button"
        className={
          "home-filter-chip" + (filter.kind === "all" ? " active" : "")
        }
        onClick={() => onChange({ kind: "all" })}
      >
        All <span className="actions-filter-count">{counts.all}</span>
      </button>
      {selfId && (
        <button
          type="button"
          className={
            "home-filter-chip" + (filter.kind === "mine" ? " active" : "")
          }
          onClick={() => onChange({ kind: "mine" })}
        >
          Assigned to me{" "}
          <span className="actions-filter-count">{counts.mine}</span>
        </button>
      )}
      <button
        type="button"
        className={
          "home-filter-chip" +
          (filter.kind === "unassigned" ? " active" : "")
        }
        onClick={() => onChange({ kind: "unassigned" })}
      >
        Unassigned{" "}
        <span className="actions-filter-count">{counts.unassigned}</span>
      </button>
      {others.length > 0 && (
        <select
          className={
            "home-filter-chip actions-filter-select" +
            (filter.kind === "member" ? " active" : "")
          }
          value={memberSelected?.id ?? ""}
          onChange={(e) => {
            const id = e.target.value;
            if (id) onChange({ kind: "member", memberId: id });
            else onChange({ kind: "all" });
          }}
          title="Filter by teammate"
        >
          <option value="">Teammate…</option>
          {others.map((m) => (
            <option key={m.id} value={m.id}>
              {m.display_name}
            </option>
          ))}
        </select>
      )}
      {sortedWorkstreams.length > 0 && (
        <select
          className={
            "home-filter-chip actions-filter-select" +
            (filter.kind === "workstream" ? " active" : "")
          }
          value={workstreamSelected?.id ?? ""}
          onChange={(e) => {
            const id = e.target.value;
            if (id) onChange({ kind: "workstream", workstreamId: id });
            else onChange({ kind: "all" });
          }}
          title="Filter by workstream"
        >
          <option value="">Workstream…</option>
          {sortedWorkstreams.map((w) => (
            <option key={w.id} value={w.id}>
              {w.title}
            </option>
          ))}
        </select>
      )}
    </div>
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
  onDelete,
  onOpenNote,
  onOpenWorkstream,
  members,
  workstreams,
  onReassign,
  onReattachWorkstream,
  onUndoAutoResolved,
}: {
  it: ActionListItem;
  onToggle: (id: string, nextDone: boolean) => void;
  /** When provided, a hover-revealed trash button is rendered (#100).
   *  The actions page wires this through; per-note teasers and Team
   *  reuse the row without delete. */
  onDelete?: (id: string) => void;
  onOpenNote: (path: string) => void;
  /** Optional — routes synth-origin rows to the Workstreams detail
   *  view. When omitted, synth rows still render but the click
   *  handler short-circuits. */
  onOpenWorkstream?: (id: string) => void;
  members: TeamMember[];
  /** Active workstreams for the attach chip (#111). When omitted the
   *  workstream chip is hidden. */
  workstreams?: Workstream[];
  onReassign: (actionId: string, memberId: string | null) => void;
  /** Pin/unpin the action to a workstream (#111). When omitted the
   *  chip is read-only (no popover affordance). */
  onReattachWorkstream?: (
    actionId: string,
    workstreamId: string | null,
  ) => void;
  /** Click target for the "Margin auto-resolved" pill (#124). When
   *  omitted, the pill is still rendered but read-only — used by
   *  surfaces that don't expose the Undo action. */
  onUndoAutoResolved?: (id: string) => void;
}) {
  // When the action has a resolved assignee, hide the literal
  // `Owner — ` prefix in the displayed text — the chip already
  // communicates ownership. Unresolved/ambiguous prefixes stay visible
  // so the user can see (and edit) the raw line content.
  const displayText = it.assignee_id
    ? stripLeadingOwnerPrefix(it.text)
    : it.text;
  const isSynth = it.origin_kind === "synth";
  const isWaiting =
    it.origin_synth_kind === "email_waiting"
    || it.origin_synth_kind === "teams_waiting"
    || it.origin_synth_kind === "meeting_waiting";
  const counterparty = isWaiting && it.subject_member_id
    ? members.find((m) => m.id === it.subject_member_id) ?? null
    : null;
  const openTarget = () => {
    // Origin drives the primary navigation target: note-origin rows
    // open their source note; synth-origin rows open the attached
    // workstream when present.
    if (isSynth) {
      if (it.workstream_id && onOpenWorkstream) {
        onOpenWorkstream(it.workstream_id);
      }
    } else if (it.origin_note_path) {
      onOpenNote(it.origin_note_path);
    }
  };
  const originTitle = isSynth ? it.workstream_title : it.note_title;
  const titleAttr = originTitle ? `Open ${originTitle}` : undefined;
  return (
    <div
      className="home-action-row"
      role="button"
      tabIndex={0}
      onClick={openTarget}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          openTarget();
        }
      }}
      title={titleAttr}
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
        <div className={"home-action-text" + (it.done ? " done" : "")}>{displayText}</div>
        {originTitle && (
          <div className="home-action-origin">
            {isSynth ? (
              <IconBriefcase size={10} sw={1.7} />
            ) : (
              <IconFileText size={10} sw={1.7} />
            )}
            <span className="home-action-origin-label">
              {isSynth ? "Workstream" : "Note"}
            </span>
            <span className="home-action-origin-sep">·</span>
            <span className="home-action-origin-title">{originTitle}</span>
            {/* Note-origin row pinned to a workstream: show both. */}
            {!isSynth && it.workstream_title && (
              <>
                <span className="home-action-origin-sep">·</span>
                <IconBriefcase size={10} sw={1.7} />
                <span className="home-action-origin-title">
                  {it.workstream_title}
                </span>
              </>
            )}
          </div>
        )}
        {isWaiting && (
          <div className="home-action-waiting">
            {it.origin_synth_kind === "email_waiting" && (
              <IconMail size={10} sw={1.7} />
            )}
            {it.origin_synth_kind === "teams_waiting" && (
              <IconChat size={10} sw={1.7} />
            )}
            {it.origin_synth_kind === "meeting_waiting" && (
              <IconCalendar size={10} sw={1.7} />
            )}
            {counterparty ? (
              <>
                <span className="home-action-waiting-label">from</span>
                <button
                  type="button"
                  className="home-action-waiting-name"
                  title={`Open ${counterparty.display_name}'s profile`}
                  onClick={(e) => {
                    // Stop the row's outer click so we navigate to the
                    // member instead of the row's origin target.
                    e.stopPropagation();
                    window.dispatchEvent(
                      new CustomEvent("margin:open-team-member", {
                        detail: { memberId: counterparty.id },
                      }),
                    );
                  }}
                >
                  {counterparty.display_name}
                </button>
              </>
            ) : (
              <span className="home-action-waiting-label">
                from this person
              </span>
            )}
            <span className="home-action-origin-sep">·</span>
            <span className="home-action-waiting-time">
              {timeAgo(it.created_ms, Date.now())}
            </span>
          </div>
        )}
        {typeof it.auto_resolved_ms === "number" && onUndoAutoResolved && (
          <button
            type="button"
            className="home-auto-resolved"
            title="Auto-resolved because the model judged it complete. Click to reopen."
            onClick={(e) => {
              e.stopPropagation();
              onUndoAutoResolved(it.id);
            }}
          >
            Auto-resolved · undo
          </button>
        )}
      </div>
      <DueChip dueMs={it.due_ms} />
      {workstreams && onReattachWorkstream && (
        <WorkstreamChip
          workstreamId={it.workstream_id}
          workstreamTitle={it.workstream_title}
          workstreams={workstreams}
          onPick={(wsId) => onReattachWorkstream(it.id, wsId)}
        />
      )}
      <AssigneeChip
        assigneeId={it.assignee_id}
        assigneeDisplayName={it.assignee_display_name}
        members={members}
        onPick={(memberId) => onReassign(it.id, memberId)}
      />
      {onDelete && (
        <button
          type="button"
          className="home-action-delete"
          aria-label="Delete action item"
          title="Delete"
          onClick={(e) => {
            e.stopPropagation();
            onDelete(it.id);
          }}
        >
          <IconTrash size={14} sw={1.7} />
        </button>
      )}
    </div>
  );
}

// ---- Inbox composer -----------------------------------------------------

function InboxComposerForm({
  onAdd,
  onClose,
}: {
  onAdd: (text: string, dueToken: string | null) => Promise<void>;
  onClose: () => void;
}) {
  const [text, setText] = useState("");
  const [dateStr, setDateStr] = useState("");
  const [includeTime, setIncludeTime] = useState(false);
  const [timeStr, setTimeStr] = useState("09:00");
  const [busy, setBusy] = useState(false);
  const inputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

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
            reset();
            onClose();
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
            reset();
            onClose();
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

type ActionsFilter =
  | { kind: "all" }
  | { kind: "mine" }
  | { kind: "unassigned" }
  | { kind: "member"; memberId: string }
  | { kind: "workstream"; workstreamId: string };

function ActionsFeed({
  actions,
  onToggle,
  onDelete,
  onOpenNote,
  onOpenWorkstream,
  onAddInboxTodo,
  members,
  workstreams,
  onReassign,
  onReattachWorkstream,
  onUndoAutoResolved,
}: {
  actions: ActionListItem[];
  onToggle: (id: string, nextDone: boolean) => void;
  onDelete: (id: string) => void;
  onOpenNote: (path: string) => void;
  onOpenWorkstream: (id: string) => void;
  onAddInboxTodo: (text: string, dueToken: string | null) => Promise<void>;
  members: TeamMember[];
  workstreams: Workstream[];
  onReassign: (actionId: string, memberId: string | null) => void;
  onReattachWorkstream: (
    actionId: string,
    workstreamId: string | null,
  ) => void;
  /** Reopen + lock a worker auto-resolved row (#124). */
  onUndoAutoResolved: (id: string) => void;
}) {
  const [composerOpen, setComposerOpen] = useState(false);
  const [filter, setFilter] = useState<ActionsFilter>({ kind: "all" });
  // The "New todo" trigger lives in PageHeader (when nav === "actions");
  // we listen for its dispatched event and open the inline composer.
  useEffect(() => {
    const onOpen = () => setComposerOpen(true);
    window.addEventListener("margin:open-actions-composer", onOpen);
    return () =>
      window.removeEventListener("margin:open-actions-composer", onOpen);
  }, []);

  const selfId = useMemo(
    () => members.find((m) => m.is_self)?.id ?? null,
    [members],
  );

  // Filter the action list client-side. The backend supports
  // assignee_id and workstream_id filter params, but routing the
  // predicate here keeps chip switches snappy and avoids extra IPC
  // round-trips.
  const filtered = useMemo(() => {
    switch (filter.kind) {
      case "all":
        return actions;
      case "mine":
        return selfId
          ? actions.filter((a) => a.assignee_id === selfId)
          : [];
      case "unassigned":
        return actions.filter((a) => a.assignee_id == null);
      case "member":
        return actions.filter((a) => a.assignee_id === filter.memberId);
      case "workstream":
        return actions.filter((a) => a.workstream_id === filter.workstreamId);
    }
  }, [actions, filter, selfId]);

  // Counts power the chip labels.
  const counts = useMemo(() => {
    let mine = 0;
    let unassigned = 0;
    for (const a of actions) {
      if (a.assignee_id == null) unassigned++;
      else if (a.assignee_id === selfId) mine++;
    }
    return { mine, unassigned, all: actions.length };
  }, [actions, selfId]);

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
    for (const a of filtered) {
      if (a.due_ms != null) {
        buckets[dueBucket(a.due_ms, now)].push(a);
      } else {
        undatedRows.push(a);
      }
    }
    return { byBucket: buckets, undated: undatedRows };
  }, [filtered]);

  // Page-level "New todo" trigger lives in PageHeader (Home.tsx);
  // ActionsFeed renders only the composer form when open.
  const composer = composerOpen ? (
    <InboxComposerForm
      onAdd={onAddInboxTodo}
      onClose={() => setComposerOpen(false)}
    />
  ) : null;

  const filterBar = (
    <ActionsFilterBar
      filter={filter}
      onChange={setFilter}
      members={members}
      workstreams={workstreams}
      selfId={selfId}
      counts={counts}
    />
  );

  if (actions.length === 0) {
    return (
      <section className="home-section">
        {composer}
        <p className="home-empty">
          No open action items. Add <code>- [ ] task</code> lines to any note,
          trail with <code>@2026-01-15</code> / <code>@tomorrow</code> /{" "}
          <code>@friday</code> to schedule, or hit <em>New todo</em> above.
        </p>
      </section>
    );
  }

  if (filtered.length === 0) {
    return (
      <section className="home-section">
        {composer}
        {filterBar}
        <p className="home-empty">
          No action items match this filter.{" "}
          <button
            type="button"
            className="home-empty-link"
            onClick={() => setFilter({ kind: "all" })}
          >
            Clear filter
          </button>
        </p>
      </section>
    );
  }

  return (
    <section className="home-section">
      {composer}
      {filterBar}

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
                  onDelete={onDelete}
                  onOpenNote={onOpenNote}
                  onOpenWorkstream={onOpenWorkstream}
                  members={members}
                  workstreams={workstreams}
                  onReassign={onReassign}
                  onReattachWorkstream={onReattachWorkstream}
                  onUndoAutoResolved={onUndoAutoResolved}
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
                onDelete={onDelete}
                onOpenNote={onOpenNote}
                onOpenWorkstream={onOpenWorkstream}
                members={members}
                onReassign={onReassign}
                onUndoAutoResolved={onUndoAutoResolved}
              />
            ))}
          </div>
        </div>
      )}
    </section>
  );
}

// ---------- Open questions feed (#113) ------------------------------------

/// Dedicated Open Questions page — clones the data flow of
/// `ActionsFeed` but reads from `note_open_questions` via the
/// `listOpenQuestions` IPC. Top-of-page tab toggle picks open vs.
/// resolved. Asked-of and workstream filters mirror the actions
/// surface. No due-date bucketing — questions don't carry due dates
/// in v1; rows are ordered by parent note's `modified_ms desc`.
function OpenQuestionsFeed({
  onOpenNote,
  members,
  workstreams,
}: {
  onOpenNote: (path: string) => void;
  members: TeamMember[];
  workstreams: Workstream[];
}) {
  const [scope, setScope] = useState<QuestionScope>("open");
  const [items, setItems] = useState<OpenQuestionItem[]>([]);
  const [askedOfFilter, setAskedOfFilter] = useState<string | null>(null);
  const [workstreamFilter, setWorkstreamFilter] = useState<string | null>(
    null,
  );

  const refresh = useCallback(async () => {
    try {
      const list = await listOpenQuestions(
        scope,
        askedOfFilter ?? undefined,
        workstreamFilter ?? undefined,
      );
      setItems(list);
    } catch (err) {
      console.error("listOpenQuestions failed:", err);
    }
  }, [scope, askedOfFilter, workstreamFilter]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const onResolve = useCallback(
    async (id: string, answer: string | null) => {
      setItems((curr) => curr.filter((q) => q.id !== id));
      try {
        await resolveOpenQuestion(id, answer);
      } catch (err) {
        console.error("resolveOpenQuestion failed:", err);
      }
      await refresh();
    },
    [refresh],
  );

  const onReopen = useCallback(
    async (id: string) => {
      setItems((curr) => curr.filter((q) => q.id !== id));
      try {
        await reopenOpenQuestion(id);
      } catch (err) {
        console.error("reopenOpenQuestion failed:", err);
      }
      await refresh();
    },
    [refresh],
  );

  const onReassign = useCallback(
    async (id: string, memberId: string | null) => {
      setItems((curr) =>
        curr.map((q) =>
          q.id === id
            ? {
                ...q,
                asked_of_id: memberId,
                asked_of_display_name:
                  memberId === null
                    ? null
                    : members.find((m) => m.id === memberId)?.display_name ?? null,
              }
            : q,
        ),
      );
      try {
        await setOpenQuestionAskedOf(id, memberId);
      } catch (err) {
        console.error("setOpenQuestionAskedOf failed:", err);
      }
      await refresh();
    },
    [members, refresh],
  );

  const onDelete = useCallback(
    async (id: string) => {
      setItems((curr) => curr.filter((q) => q.id !== id));
      try {
        await deleteOpenQuestion(id);
      } catch (err) {
        console.error("deleteOpenQuestion failed:", err);
      }
      await refresh();
    },
    [refresh],
  );

  const filterBar = (
    <div className="actions-filter-bar">
      <button
        type="button"
        className={"home-filter-chip" + (scope === "open" ? " active" : "")}
        onClick={() => setScope("open")}
      >
        Open
      </button>
      <button
        type="button"
        className={"home-filter-chip" + (scope === "resolved" ? " active" : "")}
        onClick={() => setScope("resolved")}
      >
        Resolved
      </button>
      <span className="actions-filter-spacer" />
      {members.length > 0 && (
        <select
          className={
            "home-filter-chip actions-filter-select" +
            (askedOfFilter ? " active" : "")
          }
          value={askedOfFilter ?? ""}
          onChange={(e) => setAskedOfFilter(e.target.value || null)}
          title="Filter by asked-of"
        >
          <option value="">Asked of…</option>
          {members.map((m) => (
            <option key={m.id} value={m.id}>
              {m.display_name}
            </option>
          ))}
        </select>
      )}
      {workstreams.length > 0 && (
        <select
          className={
            "home-filter-chip actions-filter-select" +
            (workstreamFilter ? " active" : "")
          }
          value={workstreamFilter ?? ""}
          onChange={(e) => setWorkstreamFilter(e.target.value || null)}
          title="Filter by workstream"
        >
          <option value="">Workstream…</option>
          {workstreams.map((w) => (
            <option key={w.id} value={w.id}>
              {w.title}
            </option>
          ))}
        </select>
      )}
    </div>
  );

  if (items.length === 0) {
    return (
      <section className="home-section">
        {filterBar}
        <p className="home-empty">
          {scope === "open"
            ? "No open questions. Add `- [?] question` lines to any note to track unresolved questions."
            : "No resolved questions yet."}
        </p>
      </section>
    );
  }

  return (
    <section className="home-section">
      {filterBar}
      <div className="home-actions">
        {items.map((q) => (
          <OpenQuestionRow
            key={q.id}
            q={q}
            members={members}
            onOpenNote={onOpenNote}
            onResolve={(answer) => void onResolve(q.id, answer)}
            onReopen={() => void onReopen(q.id)}
            onReassign={(memberId) => void onReassign(q.id, memberId)}
            onDelete={() => void onDelete(q.id)}
          />
        ))}
      </div>
    </section>
  );
}

function OpenQuestionRow({
  q,
  members,
  onOpenNote,
  onResolve,
  onReopen,
  onReassign,
  onDelete,
}: {
  q: OpenQuestionItem;
  members: TeamMember[];
  onOpenNote: (path: string) => void;
  onResolve: (answer: string | null) => void;
  onReopen: () => void;
  onReassign: (memberId: string | null) => void;
  onDelete: () => void;
}) {
  const displayText = q.asked_of_id
    ? stripLeadingOwnerPrefix(q.text)
    : q.text;
  const open = () => q.origin_note_path && onOpenNote(q.origin_note_path);
  return (
    <div
      className="home-action-row"
      role="button"
      tabIndex={0}
      onClick={open}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          open();
        }
      }}
      title={q.note_title ? `Open ${q.note_title}` : undefined}
    >
      <ResolveQuestionPopover
        resolved={q.resolved}
        initialAnswer={q.resolved_note}
        onResolve={onResolve}
        onReopen={onReopen}
      />
      <div className="home-action-body">
        <div className={"home-action-text" + (q.resolved ? " done" : "")}>
          {displayText}
        </div>
        {q.resolved && q.resolved_note && (
          <div className="home-action-answer">
            <span className="home-action-answer-bullet">↳</span>
            <span className="home-action-answer-text">{q.resolved_note}</span>
          </div>
        )}
        {(q.note_title || q.workstream_title) && (
          <div className="home-action-origin">
            <IconFileText size={10} sw={1.7} />
            {q.note_title && (
              <>
                <span className="home-action-origin-label">Note</span>
                <span className="home-action-origin-sep">·</span>
                <span className="home-action-origin-title">{q.note_title}</span>
              </>
            )}
            {q.workstream_title && (
              <>
                <span className="home-action-origin-sep">·</span>
                <IconBriefcase size={10} sw={1.7} />
                <span className="home-action-origin-title">
                  {q.workstream_title}
                </span>
              </>
            )}
          </div>
        )}
      </div>
      <AssigneeChip
        assigneeId={q.asked_of_id}
        assigneeDisplayName={q.asked_of_display_name}
        members={members}
        onPick={(memberId) => onReassign(memberId)}
      />
      <button
        type="button"
        className="home-action-delete"
        aria-label="Delete question"
        title="Delete"
        onClick={(e) => {
          e.stopPropagation();
          onDelete();
        }}
      >
        <IconTrash size={14} sw={1.7} />
      </button>
    </div>
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
        <p className="home-empty">Nothing matches this filter.</p>
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
