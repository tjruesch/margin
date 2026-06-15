import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useEffect, useMemo, useState } from "react";

import {
  type CalendarEvent,
  type ConnectorStatusEvent,
  listCalendarEvents,
  listConnectors,
  type NoteListItem,
  openOrCreateEventNote,
} from "./file";
import { ActivityPanel } from "./ActivityPanel";
import { avatarColor } from "./initials";
import { MoreMenu } from "./MoreMenu";
import { type NotificationRecord, unreadCount } from "./notifications";
import { NotificationsPanel } from "./NotificationsPanel";
import { ChatPage } from "./ChatPage";
import { ChangelogView } from "./Changelog";
import { SearchPalette } from "./SearchPalette";
import { TeamView } from "./Team";
import { WorkstreamsView } from "./Workstreams";
import {
  IconArchive,
  IconBell,
  IconBrand,
  IconBriefcase,
  IconChevRight,
  IconHome,
  IconMic,
  IconMore,
  IconPlus,
  IconSearch,
  IconSettings,
  IconSidebar,
  IconSparkle,
  IconStar,
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
  | "chat"
  | "workstreams"
  | "changelog"
  | "favorites"
  | "archive"
  | "team";
type FilterId = "all" | "notes" | "meetings";

/// Connector-health hook (#141). Returns `true` when at least one
/// connector has a non-null `last_error` — drives the alert dot on the
/// sidebar Settings nav item so the user notices a broken sync without
/// having to open Settings → Connectors proactively. Refreshes on every
/// `connector-status` Tauri event, so the dot flips off the moment the
/// runner reports the next successful sync.
function useConnectorHealthAlert(): boolean {
  const [hasError, setHasError] = useState(false);
  useEffect(() => {
    let cancelled = false;
    const refresh = async () => {
      try {
        const list = await listConnectors();
        if (cancelled) return;
        setHasError(list.some((c) => !!c.last_error));
      } catch (e) {
        if (!cancelled) console.error("[home] listConnectors failed:", e);
      }
    };
    void refresh();
    let unlisten: UnlistenFn | null = null;
    (async () => {
      const fn = await listen<ConnectorStatusEvent>("connector-status", () => {
        void refresh();
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
  return hasError;
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
  // sender doesn't have to lift state.
  useEffect(() => {
    const VALID: NavId[] = [
      "home",
      "chat",
      "workstreams",
      "changelog",
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

  const { upcoming, raw: rawEvents, nowMs: eventsNowMs } = useUpcomingEvents();
  const settingsAlert = useConnectorHealthAlert();

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
          tags={allTags}
          activeTag={tagFilter}
          onTagSelect={(t) => setTagFilter(t === tagFilter ? null : t)}
          onOpenSettings={onOpenSettings}
          settingsAlert={settingsAlert}
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
        ) : nav === "chat" ? (
          // ChatPage renders its own header with a Clear button.
          null
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

        {nav === "chat" ? (
          <ChatPage
            onOpenNote={onOpen}
            onOpenWorkstream={openWorkstream}
          />
        ) : nav === "team" ? (
          <TeamView
            onOpenNote={onOpen}
            onOpenWorkstream={openWorkstream}
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
        ) : nav === "changelog" ? (
          <ChangelogView onOpenSettings={onOpenSettings} />
        ) : (
          <>
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
  tags,
  activeTag,
  onTagSelect,
  onOpenSettings,
  settingsAlert,
  onOpenPalette,
  onNewNote,
  onNewMeeting,
}: {
  active: NavId;
  onSelect: (id: NavId) => void;
  tags: string[];
  activeTag: string | null;
  onTagSelect: (tag: string) => void;
  onOpenSettings: () => void;
  /** Surface a small dot on the Settings nav item when any connector
   *  is errored (#141). Lets the user notice broken sync from anywhere
   *  in the app, not just by visiting Settings → Connectors. */
  settingsAlert: boolean;
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
          icon={<IconSparkle size={14} sw={1.7} />}
          label="Chat"
          active={active === "chat"}
          onClick={() => onSelect("chat")}
        />
        <NavItem
          icon={<IconBriefcase size={14} sw={1.7} />}
          label="Workstreams"
          active={active === "workstreams"}
          onClick={() => onSelect("workstreams")}
        />
        <NavItem
          icon={<IconBrand kind="github" size={14} />}
          label="Changelog"
          active={active === "changelog"}
          onClick={() => onSelect("changelog")}
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
          alert={settingsAlert}
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
  alert,
  onClick,
}: {
  icon: React.ReactNode;
  label: string;
  active: boolean;
  badge?: string | null;
  /** Tiny dot on the icon corner when something needs attention but
   *  there's no count to surface — currently the Settings entry uses
   *  this for broken-connector state (#141). Composes with `badge`. */
  alert?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      className={"home-nav-item" + (active ? " active" : "")}
      onClick={onClick}
    >
      <span className="home-nav-icon">
        {icon}
        {alert && <span className="home-nav-alert-dot" aria-hidden="true" />}
      </span>
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
 *  inside the sub-page component (TeamSection); the page header just
 *  fires the open trigger. Refresh on Workstreams has its handler
 *  already at Home.tsx scope so it's a direct call. */
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
    case "changelog":
    case "favorites":
    case "archive":
    case "home":
    case "chat":
      return null;
  }
}

function pageHeaderTitle(nav: NavId): string {
  switch (nav) {
    case "workstreams":
      return "Workstreams";
    case "changelog":
      return "Changelog";
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
    case "chat":
      // Chat renders its own header; unreachable from the call site.
      return "Chat";
  }
}

function PageHeader({
  title,
  actions,
}: {
  title: string;
  /** Page-scoped actions rendered on the right of the header. e.g.
   *  "Add team member" on Team, "Refresh" on Workstreams. The global
   *  New-note / New-meeting CTAs live in the sidebar footer for
   *  non-home pages instead of competing here. */
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
