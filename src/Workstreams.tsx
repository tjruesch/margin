//! Workstreams view (#71).
//!
//! Sidebar nav target. List of synthesized workstreams as cards;
//! click → detail view with sections for actions, emails, meetings,
//! notes. Refresh button forces a synthesis pass via the boot
//! pipeline added in #70 and listens for `workstream-status` to
//! refetch.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  type EmailMessage,
  type TeamMember,
  type Workstream,
  type WorkstreamAction,
  type WorkstreamDetail,
  type WorkstreamStatus,
  getEmailBody,
  getWorkstreamDetails,
  listArchivedWorkstreams,
  listTeamMembers,
  markWorkstreamSeen,
  openOrCreateEventNote,
  setWorkstreamActionDone,
  setWorkstreamOwner,
  setWorkstreamStatus,
  setWorkstreamUserNotes,
} from "./file";
import { IconChevLeft } from "./icons";
import { avatarColor, initialsFromName } from "./initials";

// ----- List view -----------------------------------------------------------

export function WorkstreamsView({
  workstreams,
  loading,
  synthInFlight,
  synthMessage,
  onRefresh,
  onOpenNote,
}: {
  workstreams: Workstream[];
  loading: boolean;
  synthInFlight: boolean;
  synthMessage: string | null;
  onRefresh: () => void;
  onOpenNote: (path: string) => void;
}) {
  const [selectedId, setSelectedId] = useState<string | null>(null);
  // Team-member cache for owner / member chips (#81). Loaded once and
  // refreshed on `margin:team-changed` events.
  const [teamMembers, setTeamMembers] = useState<TeamMember[]>([]);
  // Filter-by-member dropdown (#81). null = no filter (default).
  const [memberFilter, setMemberFilter] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const members = await listTeamMembers();
        if (!cancelled) setTeamMembers(members);
      } catch (e) {
        console.error("[workstreams] listTeamMembers failed", e);
      }
    })();
    const onChanged = () => {
      void listTeamMembers().then((m) => setTeamMembers(m)).catch(() => {});
    };
    window.addEventListener("margin:team-changed", onChanged);
    return () => {
      cancelled = true;
      window.removeEventListener("margin:team-changed", onChanged);
    };
  }, []);

  const teamById = useMemo(() => {
    const m = new Map<string, TeamMember>();
    for (const t of teamMembers) m.set(t.id, t);
    return m;
  }, [teamMembers]);

  // External open: the AI ask palette dispatches `margin:open-workstream`
  // when a `[W*]` citation chip is clicked (#72). This view is only
  // mounted when nav === "workstreams", so the dispatcher fires the
  // event a microtask after switching nav so we're guaranteed to be
  // listening by the time it lands.
  useEffect(() => {
    const onOpen = (e: Event) => {
      const detail = (e as CustomEvent<unknown>).detail;
      if (typeof detail === "string" && detail.length > 0) {
        setSelectedId(detail);
      }
    };
    window.addEventListener("margin:open-workstream", onOpen);
    return () => window.removeEventListener("margin:open-workstream", onOpen);
  }, []);

  // Filter dropdown options: every team member referenced as owner or
  // member on at least one active workstream. Computed via useMemo —
  // must run before any early return to satisfy the Rules of Hooks.
  const filterCandidates = useFilterCandidates(workstreams, teamById);

  if (selectedId) {
    return (
      <WorkstreamDetailView
        id={selectedId}
        onBack={() => setSelectedId(null)}
        onOpenNote={onOpenNote}
        teamMembers={teamMembers}
        teamById={teamById}
      />
    );
  }

  const filteredActive = applyMemberFilter(workstreams, memberFilter);

  const nowMs = Date.now();

  return (
    <div className="workstream-view">
      <header className="workstream-header">
        <div>
          <h1 className="workstream-title">Workstreams</h1>
          <p className="workstream-subtitle">
            Ongoing efforts synthesized from emails, meetings, and notes.
          </p>
        </div>
        <button
          type="button"
          className="workstream-refresh"
          onClick={onRefresh}
          disabled={synthInFlight}
        >
          {synthInFlight ? (
            <>
              <span className="workstream-spinner" aria-hidden />
              Synthesizing…
            </>
          ) : (
            "Refresh"
          )}
        </button>
      </header>

      {synthMessage ? (
        <div className="workstream-toast" role="status">
          {synthMessage}
        </div>
      ) : null}

      {!loading && filterCandidates.length > 0 ? (
        <div className="workstream-filter">
          <label htmlFor="workstream-member-filter">Filter by member</label>
          <select
            id="workstream-member-filter"
            value={memberFilter ?? ""}
            onChange={(e) => setMemberFilter(e.target.value || null)}
          >
            <option value="">All</option>
            {filterCandidates.map((m) => (
              <option key={m.id} value={m.id}>
                {m.display_name}
              </option>
            ))}
          </select>
        </div>
      ) : null}

      {loading ? (
        <p className="home-empty">Loading…</p>
      ) : workstreams.length === 0 ? (
        <div className="workstream-empty">
          <p>No workstreams yet.</p>
          <p>
            Connect Microsoft in Settings to ingest your inbox and calendar,
            then click Refresh to synthesize.
          </p>
        </div>
      ) : filteredActive.length === 0 ? (
        <p className="home-empty">
          No active workstreams match this filter.
        </p>
      ) : (
        <div className="workstream-list">
          {filteredActive.map((w) => (
            <WorkstreamCard
              key={w.id}
              workstream={w}
              nowMs={nowMs}
              onClick={() => setSelectedId(w.id)}
              teamById={teamById}
            />
          ))}
        </div>
      )}

      {!loading && (
        <ArchivedSection
          onSelect={(id) => setSelectedId(id)}
          nowMs={nowMs}
          synthInFlight={synthInFlight}
          teamById={teamById}
          memberFilter={memberFilter}
        />
      )}
    </div>
  );
}

function useFilterCandidates(
  workstreams: Workstream[],
  teamById: Map<string, TeamMember>,
): TeamMember[] {
  return useMemo(() => {
    const ids = new Set<string>();
    for (const w of workstreams) {
      if (w.owner_member_id) ids.add(w.owner_member_id);
      for (const m of w.members) ids.add(m);
    }
    const out: TeamMember[] = [];
    for (const id of ids) {
      const m = teamById.get(id);
      if (m) out.push(m);
    }
    out.sort((a, b) => a.display_name.localeCompare(b.display_name));
    return out;
  }, [workstreams, teamById]);
}

function applyMemberFilter<T extends Workstream>(
  workstreams: T[],
  memberId: string | null,
): T[] {
  if (!memberId) return workstreams;
  return workstreams.filter(
    (w) => w.owner_member_id === memberId || w.members.includes(memberId),
  );
}

function WorkstreamCard({
  workstream: w,
  nowMs,
  onClick,
  teamById,
}: {
  workstream: Workstream;
  nowMs: number;
  onClick: () => void;
  teamById: Map<string, TeamMember>;
}) {
  const isReopened = w.reopened_at_ms != null && w.status === "active";
  const owner = w.owner_member_id ? teamById.get(w.owner_member_id) : null;
  // +N badge counts members beyond the owner.
  const otherMemberCount = w.members.filter(
    (id) => id !== w.owner_member_id,
  ).length;
  return (
    <button type="button" className="workstream-card" onClick={onClick}>
      <div className="workstream-card-head">
        <span className="workstream-card-title">
          {w.title}
          {isReopened ? (
            <span className="workstream-card-reopened" aria-label="Reopened">
              Reopened
            </span>
          ) : null}
        </span>
        <span className="workstream-card-people">
          {owner ? (
            <span
              className="workstream-card-owner"
              title={`Owner: ${owner.display_name}`}
              style={{ background: avatarColor(owner.display_name) }}
            >
              {initialsFromName(owner.display_name)}
            </span>
          ) : null}
          {otherMemberCount > 0 ? (
            <span
              className="workstream-card-members"
              title={`${otherMemberCount} other member${otherMemberCount === 1 ? "" : "s"}`}
            >
              +{otherMemberCount}
            </span>
          ) : null}
          <span className="workstream-card-time">
            {formatPast(w.last_activity_ms, nowMs)}
          </span>
        </span>
      </div>
      <p className="workstream-card-summary">{w.summary}</p>
      <div className="workstream-card-counts">{countLine(w)}</div>
    </button>
  );
}

/// Collapsed accordion at the bottom of the Workstreams list. Loads
/// archived workstreams on mount + whenever the synthesizer finishes
/// (resurrected ones drop off the archived list and reappear in
/// active). Lazy expansion would also work but archived sets are small
/// in practice; eager keeps the count badge accurate without wiring an
/// extra click-through fetch.
function ArchivedSection({
  onSelect,
  nowMs,
  synthInFlight,
  teamById,
  memberFilter,
}: {
  onSelect: (id: string) => void;
  nowMs: number;
  synthInFlight: boolean;
  teamById: Map<string, TeamMember>;
  memberFilter: string | null;
}) {
  const [archived, setArchived] = useState<Workstream[]>([]);
  const [expanded, setExpanded] = useState(false);

  const reload = useCallback(async () => {
    try {
      setArchived(await listArchivedWorkstreams());
    } catch (e) {
      console.error("[workstreams] listArchivedWorkstreams failed", e);
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  // Refetch whenever a synthesis pass finishes — a resurrected
  // workstream's status flips from archived → active.
  useEffect(() => {
    if (!synthInFlight) {
      void reload();
    }
  }, [synthInFlight, reload]);

  const filtered = applyMemberFilter(archived, memberFilter);

  if (archived.length === 0) return null;
  return (
    <section className="workstream-archived-section">
      <button
        type="button"
        className="workstream-archived-toggle"
        onClick={() => setExpanded((v) => !v)}
        aria-expanded={expanded}
      >
        <span>{expanded ? "▾" : "▸"}</span>
        Archived ({memberFilter ? `${filtered.length}/${archived.length}` : archived.length})
      </button>
      {expanded ? (
        filtered.length === 0 ? (
          <p className="home-empty">No archived workstreams match this filter.</p>
        ) : (
          <div className="workstream-archived-list">
            {filtered.map((w) => (
              <WorkstreamCard
                key={w.id}
                workstream={w}
                nowMs={nowMs}
                onClick={() => onSelect(w.id)}
                teamById={teamById}
              />
            ))}
          </div>
        )
      ) : null}
    </section>
  );
}

function countLine(w: Workstream): string {
  const parts: string[] = [];
  if (w.open_action_count > 0)
    parts.push(plural(w.open_action_count, "open action", "open actions"));
  if (w.email_count > 0) parts.push(plural(w.email_count, "email", "emails"));
  if (w.event_count > 0)
    parts.push(plural(w.event_count, "meeting", "meetings"));
  if (w.note_count > 0) parts.push(plural(w.note_count, "note", "notes"));
  return parts.length ? parts.join(" · ") : "No items yet";
}

function plural(n: number, singular: string, plural_: string): string {
  return `${n} ${n === 1 ? singular : plural_}`;
}

// ----- Detail view ---------------------------------------------------------

function WorkstreamDetailView({
  id,
  onBack,
  onOpenNote,
  teamMembers,
  teamById,
}: {
  id: string;
  onBack: () => void;
  onOpenNote: (path: string) => void;
  teamMembers: TeamMember[];
  teamById: Map<string, TeamMember>;
}) {
  const [detail, setDetail] = useState<WorkstreamDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [missing, setMissing] = useState(false);

  const reload = useCallback(async () => {
    setLoading(true);
    try {
      const d = await getWorkstreamDetails(id);
      if (!d) {
        setMissing(true);
        setDetail(null);
      } else {
        setMissing(false);
        setDetail(d);
      }
    } catch (e) {
      console.error("[workstreams] detail fetch failed", e);
      setMissing(true);
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    void reload();
  }, [reload]);

  // Reopened-marker clearing (#78). When the user opens a workstream
  // that was just reopened by the synthesizer, fire markWorkstreamSeen
  // on UNMOUNT so the user has the entire detail-view lifetime to see
  // the badge before it clears. The unmount cleanup runs when the user
  // navigates back, switches to a different workstream, or leaves the
  // Workstreams view entirely.
  const reopenedRef = useRef<{ id: string; needsClear: boolean }>({
    id,
    needsClear: false,
  });
  useEffect(() => {
    reopenedRef.current = {
      id,
      needsClear:
        !!detail &&
        detail.reopened_at_ms != null &&
        detail.status === "active",
    };
  }, [id, detail?.reopened_at_ms, detail?.status]);
  useEffect(() => {
    return () => {
      const snap = reopenedRef.current;
      if (snap.needsClear) {
        void markWorkstreamSeen(snap.id).catch((e) => {
          console.error("[workstreams] markWorkstreamSeen failed", e);
        });
      }
    };
  }, []);

  // Optimistic update for action toggle. On error, revert and refetch
  // to reconcile.
  const onToggleAction = useCallback(
    async (actionId: string, nextDone: boolean) => {
      setDetail((d) => {
        if (!d) return d;
        return {
          ...d,
          actions: d.actions.map((a) =>
            a.id === actionId ? { ...a, done: nextDone } : a,
          ),
        };
      });
      try {
        await setWorkstreamActionDone(actionId, nextDone);
      } catch (e) {
        console.error("[workstreams] toggle action failed", e);
        await reload();
      }
    },
    [reload],
  );

  const onChangeStatus = useCallback(
    async (status: WorkstreamStatus) => {
      try {
        await setWorkstreamStatus(id, status);
        if (status !== "active") {
          // Archived/snoozed workstreams drop off the list, so go back.
          onBack();
        } else {
          await reload();
        }
      } catch (e) {
        console.error("[workstreams] set status failed", e);
        await reload();
      }
    },
    [id, onBack, reload],
  );

  const onOpenEvent = useCallback(
    async (eventId: string) => {
      try {
        const path = await openOrCreateEventNote(eventId);
        onOpenNote(path);
      } catch (e) {
        console.error("[workstreams] open event note failed", e);
      }
    },
    [onOpenNote],
  );

  if (loading && !detail) {
    return (
      <div className="workstream-view">
        <DetailHeader title="" onBack={onBack} status={null} onChangeStatus={() => {}} />
        <p className="home-empty">Loading…</p>
      </div>
    );
  }
  if (missing || !detail) {
    return (
      <div className="workstream-view">
        <DetailHeader title="Workstream" onBack={onBack} status={null} onChangeStatus={() => {}} />
        <p className="home-empty">Workstream not found.</p>
      </div>
    );
  }

  return (
    <div className="workstream-view">
      <DetailHeader
        title={detail.title}
        onBack={onBack}
        status={detail.status}
        onChangeStatus={onChangeStatus}
        ownerId={detail.owner_member_id}
        teamMembers={teamMembers}
        onChangeOwner={async (ownerId) => {
          // Optimistic local update; revert on error.
          const prev = detail.owner_member_id;
          setDetail((d) => (d ? { ...d, owner_member_id: ownerId } : d));
          try {
            await setWorkstreamOwner(detail.id, ownerId);
          } catch (e) {
            console.error("[workstreams] setWorkstreamOwner failed", e);
            setDetail((d) => (d ? { ...d, owner_member_id: prev } : d));
          }
        }}
      />
      <p className="workstream-detail-summary">{detail.summary}</p>

      {detail.members.length > 0 ? (
        <MembersStrip
          memberIds={detail.members}
          ownerId={detail.owner_member_id}
          teamById={teamById}
        />
      ) : null}

      <WorkstreamUserNotes
        workstreamId={detail.id}
        initialNotes={detail.user_notes}
        onSaved={(notes) =>
          setDetail((d) => (d ? { ...d, user_notes: notes } : d))
        }
      />

      <ActionsSection actions={detail.actions} onToggle={onToggleAction} />

      <EmailsSection emails={detail.emails} />

      <MeetingsSection
        events={detail.events}
        onOpenEvent={onOpenEvent}
      />

      <NotesSection notes={detail.notes} onOpenNote={onOpenNote} />
    </div>
  );
}

// ----- User notes (#77) ----------------------------------------------------

type SaveStatus = "idle" | "saving" | "saved" | "error";

function WorkstreamUserNotes({
  workstreamId,
  initialNotes,
  onSaved,
}: {
  workstreamId: string;
  initialNotes: string | null;
  onSaved: (notes: string | null) => void;
}) {
  const [draft, setDraft] = useState<string>(initialNotes ?? "");
  const [editing, setEditing] = useState<boolean>(!!initialNotes);
  const [status, setStatus] = useState<SaveStatus>("idle");

  // Re-seed when navigating between workstreams. The detail view
  // unmount/remounts on selectedId change, but useState keeps initial
  // values across renders — guard by id.
  const seededIdRef = useRef<string>(workstreamId);
  useEffect(() => {
    if (seededIdRef.current !== workstreamId) {
      seededIdRef.current = workstreamId;
      setDraft(initialNotes ?? "");
      setEditing(!!initialNotes);
      setStatus("idle");
    }
  }, [workstreamId, initialNotes]);

  // Latest persisted text — used for revert-on-error and to detect
  // no-op saves.
  const persistedRef = useRef<string | null>(initialNotes);
  // Latest workstream id at fire time so a debounced save that resolves
  // after the user navigates away doesn't mis-patch a different
  // workstream.
  const idRef = useRef<string>(workstreamId);
  useEffect(() => {
    idRef.current = workstreamId;
  }, [workstreamId]);

  const save = useCallback(
    async (text: string) => {
      const idAtFire = idRef.current;
      const normalized = text.trim().length === 0 ? null : text;
      if (normalized === persistedRef.current) {
        return; // no-op, draft matches DB
      }
      setStatus("saving");
      try {
        await setWorkstreamUserNotes(idAtFire, normalized);
        // If the user navigated to a different workstream while this
        // was in flight, drop the patch on the floor.
        if (idAtFire !== idRef.current) return;
        persistedRef.current = normalized;
        onSaved(normalized);
        setStatus("saved");
      } catch (e) {
        console.error("[workstreams] save user notes failed", e);
        if (idAtFire !== idRef.current) return;
        setStatus("error");
      }
    },
    [onSaved],
  );

  // 600ms debounce on draft change. Fires when the user pauses typing.
  useEffect(() => {
    if (!editing) return;
    if ((draft || "") === (persistedRef.current ?? "")) return;
    const t = window.setTimeout(() => {
      void save(draft);
    }, 600);
    return () => window.clearTimeout(t);
  }, [draft, editing, save]);

  // Auto-clear the "saved" indicator after a beat so it doesn't sit
  // there permanently.
  useEffect(() => {
    if (status !== "saved") return;
    const t = window.setTimeout(() => setStatus("idle"), 1500);
    return () => window.clearTimeout(t);
  }, [status]);

  if (!editing) {
    return (
      <section className="workstream-user-notes is-empty">
        <button
          type="button"
          className="workstream-user-notes-add-link"
          onClick={() => setEditing(true)}
        >
          Add context…
        </button>
      </section>
    );
  }

  return (
    <section className="workstream-user-notes">
      <div className="workstream-user-notes-head">
        <span className="workstream-user-notes-label">Your notes</span>
        <span
          className={`workstream-user-notes-status status-${status}`}
          aria-live="polite"
        >
          {status === "saving"
            ? "Saving…"
            : status === "saved"
            ? "Saved"
            : status === "error"
            ? "Couldn't save — try again"
            : ""}
        </span>
      </div>
      <textarea
        className="workstream-user-notes-textarea"
        value={draft}
        placeholder="Real deadline, internal owner, dollar value, scope clarifications…"
        onChange={(e) => setDraft(e.target.value)}
        onBlur={() => void save(draft)}
        rows={4}
      />
    </section>
  );
}

function DetailHeader({
  title,
  onBack,
  status,
  onChangeStatus,
  ownerId,
  teamMembers,
  onChangeOwner,
}: {
  title: string;
  onBack: () => void;
  status: WorkstreamStatus | null;
  onChangeStatus: (s: WorkstreamStatus) => void | Promise<void>;
  ownerId?: string | null;
  teamMembers?: TeamMember[];
  onChangeOwner?: (ownerId: string | null) => void | Promise<void>;
}) {
  return (
    <header className="workstream-header workstream-detail-header">
      <button
        type="button"
        className="workstream-back"
        onClick={onBack}
        aria-label="Back to workstreams"
      >
        <IconChevLeft size={20} />
        Back
      </button>
      <h1 className="workstream-title">{title}</h1>
      {teamMembers && onChangeOwner ? (
        <select
          className="workstream-owner-select"
          value={ownerId ?? ""}
          onChange={(e) => onChangeOwner(e.target.value || null)}
          aria-label="Workstream owner"
        >
          <option value="">Unassigned</option>
          {teamMembers.map((m) => (
            <option key={m.id} value={m.id}>
              {m.display_name}
            </option>
          ))}
        </select>
      ) : null}
      {status ? (
        <select
          className="workstream-status"
          value={status}
          onChange={(e) => onChangeStatus(e.target.value as WorkstreamStatus)}
          aria-label="Workstream status"
        >
          <option value="active">Active</option>
          <option value="snoozed">Snoozed</option>
          <option value="archived">Archived</option>
        </select>
      ) : null}
    </header>
  );
}

function MembersStrip({
  memberIds,
  ownerId,
  teamById,
}: {
  memberIds: string[];
  ownerId: string | null;
  teamById: Map<string, TeamMember>;
}) {
  // Owner first (bigger chip), then everyone else.
  const ordered: TeamMember[] = [];
  const seen = new Set<string>();
  if (ownerId) {
    const m = teamById.get(ownerId);
    if (m) {
      ordered.push(m);
      seen.add(m.id);
    }
  }
  for (const id of memberIds) {
    if (seen.has(id)) continue;
    const m = teamById.get(id);
    if (m) {
      ordered.push(m);
      seen.add(m.id);
    }
  }
  if (ordered.length === 0) return null;
  return (
    <section className="workstream-members-strip">
      {ordered.map((m) => {
        const isOwner = m.id === ownerId;
        return (
          <span
            key={m.id}
            className={`workstream-member-chip ${isOwner ? "is-owner" : ""}`}
            title={isOwner ? `${m.display_name} (owner)` : m.display_name}
          >
            <span
              className="workstream-member-avatar"
              style={{ background: avatarColor(m.display_name) }}
            >
              {initialsFromName(m.display_name)}
            </span>
            <span className="workstream-member-name">{m.display_name}</span>
          </span>
        );
      })}
    </section>
  );
}

// ----- Sections ------------------------------------------------------------

function ActionsSection({
  actions,
  onToggle,
}: {
  actions: WorkstreamAction[];
  onToggle: (actionId: string, nextDone: boolean) => void | Promise<void>;
}) {
  if (actions.length === 0) return null;
  return (
    <section className="workstream-section">
      <h2 className="workstream-section-title">Actions ({actions.length})</h2>
      <ul className="workstream-actions">
        {actions.map((a) => (
          <li
            key={a.id}
            className={`workstream-action-row ${a.done ? "is-done" : ""}`}
          >
            <label className="workstream-action-check">
              <input
                type="checkbox"
                checked={a.done}
                onChange={(e) => onToggle(a.id, e.target.checked)}
              />
              <span>{a.text}</span>
            </label>
            <span className="workstream-action-source">
              from {a.source_kind}
            </span>
          </li>
        ))}
      </ul>
    </section>
  );
}

function EmailsSection({ emails }: { emails: EmailMessage[] }) {
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [bodies, setBodies] = useState<Record<string, BodyState>>({});

  if (emails.length === 0) return null;

  const toggle = async (m: EmailMessage) => {
    const isOpen = !!expanded[m.id];
    setExpanded({ ...expanded, [m.id]: !isOpen });
    if (isOpen) return;
    if (bodies[m.id]) return; // already loaded or loading

    if (m.body_html) {
      setBodies((b) => ({ ...b, [m.id]: { kind: "loaded", html: m.body_html! } }));
      return;
    }
    setBodies((b) => ({ ...b, [m.id]: { kind: "loading" } }));
    try {
      const html = await getEmailBody(m.id);
      if (html) {
        setBodies((b) => ({ ...b, [m.id]: { kind: "loaded", html } }));
      } else {
        setBodies((b) => ({
          ...b,
          [m.id]: { kind: "empty" },
        }));
      }
    } catch (e) {
      console.error("[workstreams] email body fetch failed", e);
      setBodies((b) => ({
        ...b,
        [m.id]: { kind: "error" },
      }));
    }
  };

  return (
    <section className="workstream-section">
      <h2 className="workstream-section-title">Emails ({emails.length})</h2>
      <ul className="workstream-emails">
        {emails.map((m) => {
          const open = !!expanded[m.id];
          const body = bodies[m.id];
          const date = formatShortDate(m.sent_at_ms);
          const fromLabel = m.from_name || m.from_email;
          return (
            <li key={m.id} className="workstream-email">
              <button
                type="button"
                className="workstream-email-row"
                onClick={() => void toggle(m)}
                aria-expanded={open}
              >
                <span className="workstream-email-date">{date}</span>
                <span className="workstream-email-from">{fromLabel}</span>
                <span className="workstream-email-subject">{m.subject}</span>
                <span className="workstream-email-chev">{open ? "▾" : "▸"}</span>
              </button>
              {open ? (
                <div className="workstream-email-body">
                  <EmailBodyPanel body={body} fallbackPreview={m.body_preview} />
                </div>
              ) : null}
            </li>
          );
        })}
      </ul>
    </section>
  );
}

type BodyState =
  | { kind: "loading" }
  | { kind: "loaded"; html: string }
  | { kind: "empty" }
  | { kind: "error" };

function EmailBodyPanel({
  body,
  fallbackPreview,
}: {
  body: BodyState | undefined;
  fallbackPreview: string | null;
}) {
  if (!body || body.kind === "loading") {
    return <p className="workstream-email-loading">Loading…</p>;
  }
  if (body.kind === "loaded") {
    // Render in a sandboxed iframe so any leftover script in the
    // email's HTML can't reach Margin's main DOM. `allow-scripts` is
    // deliberately omitted; outbound links are dead-ends in v1.
    return (
      <iframe
        title="Email body"
        className="workstream-email-iframe"
        sandbox="allow-same-origin"
        srcDoc={body.html}
      />
    );
  }
  if (body.kind === "error") {
    return (
      <p className="workstream-email-loading">
        Couldn't load email body — it may have been deleted.
        {fallbackPreview ? (
          <>
            <br />
            <span>Preview: {fallbackPreview}</span>
          </>
        ) : null}
      </p>
    );
  }
  // empty
  return (
    <p className="workstream-email-loading">
      {fallbackPreview ?? "(no body)"}
    </p>
  );
}

function MeetingsSection({
  events,
  onOpenEvent,
}: {
  events: WorkstreamDetail["events"];
  onOpenEvent: (eventId: string) => void | Promise<void>;
}) {
  if (events.length === 0) return null;
  return (
    <section className="workstream-section">
      <h2 className="workstream-section-title">Meetings ({events.length})</h2>
      <ul className="workstream-meetings">
        {events.map((e) => (
          <li key={e.id}>
            <button
              type="button"
              className="workstream-meeting-row"
              onClick={() => void onOpenEvent(e.id)}
            >
              <span className="workstream-meeting-date">
                {formatShortDateTime(e.start_ms)}
              </span>
              <span className="workstream-meeting-title">{e.title}</span>
              {e.attendees.length > 0 ? (
                <span className="workstream-meeting-attendees">
                  {e.attendees
                    .slice(0, 4)
                    .map((a) => a.display_name || a.email)
                    .join(", ")}
                  {e.attendees.length > 4
                    ? ` +${e.attendees.length - 4}`
                    : ""}
                </span>
              ) : null}
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}

function NotesSection({
  notes,
  onOpenNote,
}: {
  notes: WorkstreamDetail["notes"];
  onOpenNote: (path: string) => void;
}) {
  if (notes.length === 0) return null;
  return (
    <section className="workstream-section">
      <h2 className="workstream-section-title">Notes ({notes.length})</h2>
      <ul className="workstream-notes">
        {notes.map((n) => (
          <li key={n.note_path}>
            <button
              type="button"
              className="workstream-note-row"
              onClick={() => onOpenNote(n.note_path)}
            >
              <span className="workstream-note-date">
                {formatShortDate(n.modified_ms)}
              </span>
              <span className="workstream-note-title">
                {n.title || n.note_path}
              </span>
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}

// ----- Time helpers --------------------------------------------------------

function formatPast(ms: number, nowMs: number): string {
  const delta = nowMs - ms;
  if (delta < 60_000) return "just now";
  const min = Math.floor(delta / 60_000);
  if (min < 60) return `${min}m ago`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr}h ago`;
  const days = Math.floor(hr / 24);
  if (days === 1) return "yesterday";
  if (days < 7) return `${days}d ago`;
  return formatShortDate(ms);
}

function formatShortDate(ms: number): string {
  if (!ms) return "";
  return new Intl.DateTimeFormat([], {
    month: "short",
    day: "numeric",
  }).format(new Date(ms));
}

function formatShortDateTime(ms: number): string {
  if (!ms) return "";
  return new Intl.DateTimeFormat([], {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  }).format(new Date(ms));
}

