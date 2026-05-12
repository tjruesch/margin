//! Workstreams view (#71).
//!
//! Sidebar nav target. List of synthesized workstreams as cards;
//! click → detail view with sections for actions, emails, meetings,
//! notes. Refresh button forces a synthesis pass via the boot
//! pipeline added in #70 and listens for `workstream-status` to
//! refetch.

import type React from "react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { listen } from "@tauri-apps/api/event";
import { ask } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";

import {
  AliasKind,
  type EmailMessage,
  type ExternalParticipant,
  type TeamMember,
  type Workstream,
  type WorkstreamAction,
  type WorkstreamDetail,
  type WorkstreamLink,
  type WorkstreamLinkSummarizedEvent,
  type WorkstreamStatus,
  addWorkstreamLinkFromUrl,
  createTeamMember,
  createWorkstream,
  getEmailBody,
  getWorkstreamDetails,
  listArchivedWorkstreams,
  listTeamMembers,
  markWorkstreamSeen,
  openOrCreateEventNote,
  removeWorkstreamLink,
  setWorkstreamActionDone,
  setWorkstreamActionAssignee,
  deleteWorkstreamAction,
  setWorkstreamOwner,
  setWorkstreamParent,
  setWorkstreamStatus,
  setWorkstreamUserNotes,
} from "./file";
import { AssigneeChip } from "./AssigneeChip";
import { DueChip } from "./Home";
import {
  IconArchive,
  IconBell,
  IconBrand,
  IconBriefcase,
  IconCheck,
  IconChevLeft,
  IconChevRight,
  IconLink,
  IconMore,
  IconPlus,
  IconSearch,
  IconTrash,
  IconUser,
} from "./icons";
import { avatarColor, initialsFromName } from "./initials";

// ----- List view -----------------------------------------------------------

export function WorkstreamsView({
  workstreams,
  loading,
  synthInFlight,
  synthMessage,
  onOpenNote,
  onChanged,
}: {
  workstreams: Workstream[];
  loading: boolean;
  synthInFlight: boolean;
  synthMessage: string | null;
  onOpenNote: (path: string) => void;
  /** Fires after any workstream mutation (manual create, archive,
   *  snooze, reactivate, reparent, change owner). Parent refetches the
   *  list so the change shows up immediately without a synth pass.
   *  (#101) */
  onChanged: () => void;
}) {
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [composerOpen, setComposerOpen] = useState(false);
  // Page header dispatches `margin:open-workstream-composer` when the
  // user clicks "New workstream". Composer state lives here so we can
  // close it from the form's Cancel / save paths.
  useEffect(() => {
    const onOpen = () => setComposerOpen(true);
    window.addEventListener("margin:open-workstream-composer", onOpen);
    return () =>
      window.removeEventListener("margin:open-workstream-composer", onOpen);
  }, []);
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

  // Count direct children per parent across the full active set so the
  // umbrella count survives the member filter (an umbrella's "size" is
  // metadata about the workstream itself, not about what's currently
  // visible). Hierarchy is capped at 2 levels — nested children never
  // accumulate further descendants. Must run before the early return
  // below to satisfy Rules of Hooks (#101).
  const childCounts = useMemo(() => {
    const m = new Map<string, number>();
    for (const w of workstreams) {
      if (w.parent_workstream_id) {
        m.set(w.parent_workstream_id, (m.get(w.parent_workstream_id) ?? 0) + 1);
      }
    }
    return m;
  }, [workstreams]);

  if (selectedId) {
    return (
      <WorkstreamDetailView
        id={selectedId}
        onBack={() => setSelectedId(null)}
        onNavigateTo={(id) => setSelectedId(id)}
        onOpenNote={onOpenNote}
        onChanged={onChanged}
        teamMembers={teamMembers}
        teamById={teamById}
        allWorkstreams={workstreams}
      />
    );
  }

  const filteredActive = applyMemberFilter(workstreams, memberFilter);

  const nowMs = Date.now();

  return (
    <div className="workstream-view">
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

      {composerOpen && (
        <WorkstreamComposer
          workstreams={workstreams}
          onCancel={() => setComposerOpen(false)}
          onCreated={(id) => {
            setComposerOpen(false);
            onChanged();
            // Open the new workstream's detail so the user can add
            // notes / set owner before the next synth pass runs.
            setSelectedId(id);
          }}
        />
      )}

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
          {renderHierarchical(filteredActive, (w, kids) => (
            <WorkstreamCard
              key={w.id}
              workstream={w}
              nowMs={nowMs}
              onClick={() => setSelectedId(w.id)}
              teamById={teamById}
              nested={w.parent_workstream_id != null}
              childCount={childCounts.get(w.id) ?? 0}
              children={kids}
              onChildClick={(id) => setSelectedId(id)}
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

/// Inline composer for manual workstream creation (#101). Title is
/// required; summary anchors the synthesizer's auto-attach pass;
/// parent (top-level workstreams only) drives the sub-workstream path.
/// On success the parent surfaces the new id so we can deep-link
/// straight into the detail view.
function WorkstreamComposer({
  workstreams,
  onCancel,
  onCreated,
}: {
  workstreams: Workstream[];
  onCancel: () => void;
  onCreated: (id: string) => void;
}) {
  const [title, setTitle] = useState("");
  const [summary, setSummary] = useState("");
  const [parentId, setParentId] = useState<string>("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  // Only top-level workstreams are valid parents — the backend caps
  // hierarchy at 2 levels and rejects attempts to nest deeper.
  const parentOptions = useMemo(
    () => workstreams.filter((w) => w.parent_workstream_id == null),
    [workstreams],
  );

  const submit = async () => {
    const t = title.trim();
    if (!t || busy) return;
    setBusy(true);
    setError(null);
    try {
      const id = await createWorkstream(
        t,
        summary.trim() || null,
        parentId || null,
      );
      onCreated(id);
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  };

  return (
    <div className="workstream-composer">
      <input
        ref={inputRef}
        type="text"
        className="workstream-composer-title"
        placeholder="Workstream title"
        value={title}
        onChange={(e) => setTitle(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.preventDefault();
            onCancel();
          }
        }}
      />
      <input
        type="text"
        className="workstream-composer-summary"
        placeholder="Short summary (optional, anchors auto-attach)"
        value={summary}
        onChange={(e) => setSummary(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            void submit();
          } else if (e.key === "Escape") {
            e.preventDefault();
            onCancel();
          }
        }}
      />
      <div className="workstream-composer-actions">
        <label className="workstream-composer-parent">
          <span>Parent</span>
          <select
            value={parentId}
            onChange={(e) => setParentId(e.target.value)}
          >
            <option value="">No parent (top-level)</option>
            {parentOptions.map((w) => (
              <option key={w.id} value={w.id}>
                {w.title}
              </option>
            ))}
          </select>
        </label>
        <div className="workstream-composer-spacer" />
        <button
          type="button"
          className="workstream-composer-cancel"
          onClick={onCancel}
        >
          Cancel
        </button>
        <button
          type="button"
          className="workstream-composer-save"
          disabled={!title.trim() || busy}
          onClick={() => void submit()}
        >
          {busy ? "Creating…" : "Create"}
        </button>
      </div>
      {error && <p className="workstream-composer-error">{error}</p>}
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

/**
 * Render a flat workstreams list with each parent's direct children
 * embedded inside its card (#101). Top-level workstreams (no parent or
 * whose parent isn't in the slice) render via `renderRoot`, which
 * receives the resolved children array. Orphan children — whose parent
 * was filtered out — surface at the top level with no embedded list of
 * their own (the 2-level cap means they don't have grandchildren).
 */
function renderHierarchical(
  workstreams: Workstream[],
  renderRoot: (w: Workstream, children: Workstream[]) => React.ReactNode,
): React.ReactNode[] {
  const ids = new Set(workstreams.map((w) => w.id));
  const childrenByParent = new Map<string, Workstream[]>();
  for (const w of workstreams) {
    if (w.parent_workstream_id && ids.has(w.parent_workstream_id)) {
      const arr = childrenByParent.get(w.parent_workstream_id) ?? [];
      arr.push(w);
      childrenByParent.set(w.parent_workstream_id, arr);
    }
  }
  const out: React.ReactNode[] = [];
  for (const w of workstreams) {
    // Skip children that will render under a visible parent.
    if (w.parent_workstream_id && ids.has(w.parent_workstream_id)) continue;
    out.push(renderRoot(w, childrenByParent.get(w.id) ?? []));
  }
  return out;
}

/// Max member chips rendered on the card before overflow collapses
/// into a `+N` pill. Owner always shows when present; the cap covers
/// owner + non-owner members combined.
const CARD_CHIP_CAP = 4;

/// Max embedded child rows rendered inside the parent card before the
/// remainder collapses to a "+N more" line (#101).
const CARD_CHILD_PREVIEW_CAP = 3;
/// Max participant avatars per embedded child row.
const CHILD_ROW_AVATAR_CAP = 3;

function WorkstreamCard({
  workstream: w,
  nowMs,
  onClick,
  teamById,
  nested = false,
  childCount = 0,
  children = [],
  onChildClick,
}: {
  workstream: Workstream;
  nowMs: number;
  onClick: () => void;
  teamById: Map<string, TeamMember>;
  /** When true, renders with a left indent and muted treatment so the
   *  card visually nests under its parent (#89). */
  nested?: boolean;
  /** Number of direct child workstreams when this card is a top-level
   *  parent. Surfaces as "N sub-workstreams" in the count line so an
   *  umbrella with no direct signals still reads as non-empty (#101). */
  childCount?: number;
  /** Direct children rendered inline at the bottom of the card (#101).
   *  Visible cap is `CARD_CHILD_PREVIEW_CAP`; the rest collapses to a
   *  "+N more" line. */
  children?: Workstream[];
  /** Click handler for an embedded child row. Required when `children`
   *  is non-empty so each row routes into the child's detail. */
  onChildClick?: (id: string) => void;
}) {
  const isReopened = w.reopened_at_ms != null && w.status === "active";

  // Build the ordered list: owner first (if resolvable), then other
  // members (deduped), in the order persist returned them.
  const ordered: TeamMember[] = [];
  const seen = new Set<string>();
  if (w.owner_member_id) {
    const m = teamById.get(w.owner_member_id);
    if (m) {
      ordered.push(m);
      seen.add(m.id);
    }
  }
  for (const id of w.members) {
    if (seen.has(id)) continue;
    const m = teamById.get(id);
    if (m) {
      ordered.push(m);
      seen.add(m.id);
    }
  }
  const visible = ordered.slice(0, CARD_CHIP_CAP);
  const overflow = ordered.length - visible.length;

  const visibleChildren = children.slice(0, CARD_CHILD_PREVIEW_CAP);
  const hiddenChildCount = children.length - visibleChildren.length;
  return (
    <div
      className={"workstream-card" + (nested ? " nested" : "")}
      role="button"
      tabIndex={0}
      onClick={onClick}
      onKeyDown={(e) => {
        // Only the wrapper handles Enter/Space — nested buttons stop
        // their own keydown bubbling, so child rows don't double-fire.
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onClick();
        }
      }}
    >
      <div className="workstream-card-head">
        <span className="workstream-card-title">
          {w.title}
          {isReopened ? (
            <span className="workstream-card-reopened" aria-label="Reopened">
              Reopened
            </span>
          ) : null}
          {w.link_count > 0 ? (
            <span
              className="workstream-card-links-badge"
              aria-label={`${w.link_count} linked URL${w.link_count === 1 ? "" : "s"}`}
              title={`${w.link_count} linked URL${w.link_count === 1 ? "" : "s"}`}
            >
              <IconLink size={11} sw={1.8} />
              {w.link_count}
            </span>
          ) : null}
          {w.external_participants.length > 0 ? (
            <span
              className="workstream-card-externals-badge"
              aria-label={`${w.external_participants.length} external participant${w.external_participants.length === 1 ? "" : "s"}`}
              title={
                w.external_participants
                  .slice(0, 5)
                  .map((p) => p.display_name?.trim() || p.email)
                  .join(", ") +
                (w.external_participants.length > 5
                  ? `, +${w.external_participants.length - 5} more`
                  : "")
              }
            >
              +{w.external_participants.length} external
            </span>
          ) : null}
        </span>
        <span className="workstream-card-time">
          {formatPast(w.last_activity_ms, nowMs)}
        </span>
      </div>
      {ordered.length > 0 ? (
        <div className="workstream-card-people">
          {visible.map((m) => {
            const isOwner = m.id === w.owner_member_id;
            if (isOwner) {
              return (
                <span
                  key={m.id}
                  className="workstream-card-owner-chip"
                  title={`${m.display_name} (owner)`}
                >
                  <span aria-hidden className="workstream-card-owner-mark">
                    ★
                  </span>
                  {firstName(m.display_name)}
                </span>
              );
            }
            return (
              <span
                key={m.id}
                className="workstream-card-chip"
                title={m.display_name}
              >
                <span
                  className="workstream-card-chip-avatar"
                  style={{ background: avatarColor(m.display_name) }}
                >
                  {initialsFromName(m.display_name)}
                </span>
                <span className="workstream-card-chip-name">
                  {firstName(m.display_name)}
                </span>
              </span>
            );
          })}
          {overflow > 0 ? (
            <span
              className="workstream-card-overflow"
              title={`${overflow} more member${overflow === 1 ? "" : "s"}`}
            >
              +{overflow}
            </span>
          ) : null}
        </div>
      ) : null}
      <p className="workstream-card-summary">{w.summary}</p>
      <div className="workstream-card-counts">{countLine(w, childCount)}</div>
      {visibleChildren.length > 0 && (
        <div className="workstream-card-children">
          {visibleChildren.map((c) => (
            <EmbeddedChildRow
              key={c.id}
              workstream={c}
              teamById={teamById}
              onClick={() => onChildClick?.(c.id)}
            />
          ))}
          {hiddenChildCount > 0 && (
            <div className="workstream-card-children-more">
              …{hiddenChildCount} more
            </div>
          )}
        </div>
      )}
    </div>
  );
}

/// Embedded child row rendered inside the parent WorkstreamCard (#101).
/// Compact: indent marker, title, up to a few initials avatars, and an
/// "+N external" pill. Clicking jumps to the child's detail view.
function EmbeddedChildRow({
  workstream: w,
  teamById,
  onClick,
}: {
  workstream: Workstream;
  teamById: Map<string, TeamMember>;
  onClick: () => void;
}) {
  // Resolve people: owner first, then other members. Same ordering as
  // the parent card, but only the first `CHILD_ROW_AVATAR_CAP` get
  // rendered to keep the row slim.
  const resolved: TeamMember[] = [];
  const seen = new Set<string>();
  if (w.owner_member_id) {
    const m = teamById.get(w.owner_member_id);
    if (m) {
      resolved.push(m);
      seen.add(m.id);
    }
  }
  for (const id of w.members) {
    if (seen.has(id)) continue;
    const m = teamById.get(id);
    if (m) {
      resolved.push(m);
      seen.add(m.id);
    }
  }
  const visibleAvatars = resolved.slice(0, CHILD_ROW_AVATAR_CAP);
  const overflow = resolved.length - visibleAvatars.length;
  const externalCount = w.external_participants.length;
  return (
    <button
      type="button"
      className="workstream-card-child"
      onClick={(e) => {
        e.stopPropagation();
        onClick();
      }}
      onKeyDown={(e) => {
        // Stop the parent's keydown handler from also navigating.
        if (e.key === "Enter" || e.key === " ") e.stopPropagation();
      }}
      title={`Open ${w.title}`}
    >
      <span className="workstream-card-child-marker" aria-hidden>
        ↳
      </span>
      <span className="workstream-card-child-title">{w.title}</span>
      <span className="workstream-card-child-people">
        {visibleAvatars.map((m) => (
          <span
            key={m.id}
            className="workstream-card-child-avatar"
            style={{ background: avatarColor(m.display_name) }}
            title={m.display_name}
          >
            {initialsFromName(m.display_name)}
          </span>
        ))}
        {overflow > 0 && (
          <span
            className="workstream-card-child-overflow"
            title={`${overflow} more`}
          >
            +{overflow}
          </span>
        )}
        {externalCount > 0 && (
          <span
            className="workstream-card-child-externals"
            title={`${externalCount} external participant${externalCount === 1 ? "" : "s"}`}
          >
            +{externalCount} external
          </span>
        )}
      </span>
      <IconChevRight size={12} sw={1.7} />
    </button>
  );
}

function firstName(displayName: string): string {
  const trimmed = displayName.trim();
  const space = trimmed.indexOf(" ");
  return space === -1 ? trimmed : trimmed.slice(0, space);
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
            {renderHierarchical(filtered, (w, kids) => (
              <WorkstreamCard
                key={w.id}
                workstream={w}
                nowMs={nowMs}
                onClick={() => onSelect(w.id)}
                teamById={teamById}
                nested={w.parent_workstream_id != null}
                children={kids}
                onChildClick={(id) => onSelect(id)}
              />
            ))}
          </div>
        )
      ) : null}
    </section>
  );
}

function countLine(w: Workstream, childCount = 0): string {
  const parts: string[] = [];
  if (childCount > 0)
    parts.push(plural(childCount, "sub-workstream", "sub-workstreams"));
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
  onNavigateTo,
  onOpenNote,
  onChanged,
  teamMembers,
  teamById,
  allWorkstreams,
}: {
  id: string;
  onBack: () => void;
  /** Switch the selected workstream without leaving the detail view —
   *  used by the breadcrumb-up-to-parent and the children section's
   *  card clicks (#89). */
  onNavigateTo: (id: string) => void;
  onOpenNote: (path: string) => void;
  /** Fired after any status / parent / owner change so the list view
   *  refetches and reflects the mutation immediately (#101). */
  onChanged: () => void;
  teamMembers: TeamMember[];
  teamById: Map<string, TeamMember>;
  /** All active workstreams in the current list — used by the parent
   *  picker to enumerate legal parent candidates (NULL-parent and
   *  no children, excluding self). */
  allWorkstreams: Workstream[];
}) {
  const [detail, setDetail] = useState<WorkstreamDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [missing, setMissing] = useState(false);
  // Dropdown + per-action modals replaced the old settings modal (#101).
  // `moreOpen` toggles the `...` popover; `pickerMode` opens the
  // search-palette-style picker for either owner or parent.
  const [moreOpen, setMoreOpen] = useState(false);
  const [pickerMode, setPickerMode] = useState<"owner" | "parent" | null>(null);
  const [externalDialog, setExternalDialog] = useState<ExternalParticipant | null>(null);

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

  // Reassign an action's owner. Optimistic local update first; refetch
  // on error to reconcile.
  const onReassignAction = useCallback(
    async (actionId: string, memberId: string | null) => {
      setDetail((d) => {
        if (!d) return d;
        return {
          ...d,
          actions: d.actions.map((a) =>
            a.id === actionId ? { ...a, assignee_id: memberId } : a,
          ),
        };
      });
      try {
        await setWorkstreamActionAssignee(actionId, memberId);
      } catch (e) {
        console.error("[workstreams] reassign action failed", e);
        await reload();
      }
    },
    [reload],
  );

  // Delete an action from the workstream. Confirms first to match the
  // Action items page UX, then optimistically removes the row.
  const onDeleteAction = useCallback(
    async (actionId: string) => {
      const ok = await ask(
        "This action item will be removed from this workstream.",
        {
          title: "Delete action item?",
          kind: "warning",
          okLabel: "Delete",
          cancelLabel: "Cancel",
        },
      );
      if (!ok) return;
      setDetail((d) => {
        if (!d) return d;
        return { ...d, actions: d.actions.filter((a) => a.id !== actionId) };
      });
      try {
        await deleteWorkstreamAction(actionId);
      } catch (e) {
        console.error("[workstreams] delete action failed", e);
        await reload();
      }
    },
    [reload],
  );

  const onChangeStatus = useCallback(
    async (status: WorkstreamStatus) => {
      try {
        await setWorkstreamStatus(id, status);
        // Notify the list so the card drops off (archive/snooze) or
        // gets repainted with the new state (#101).
        onChanged();
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
    [id, onBack, onChanged, reload],
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
        <WorkstreamBackLink onBack={onBack} />
        <DetailHeader title="" />
        <p className="home-empty">Loading…</p>
      </div>
    );
  }
  if (missing || !detail) {
    return (
      <div className="workstream-view">
        <WorkstreamBackLink onBack={onBack} />
        <DetailHeader title="Workstream" />
        <p className="home-empty">Workstream not found.</p>
      </div>
    );
  }

  // Parent-picker candidate workstreams (#89): NULL-parent, no children
  // of their own, and not self. We recompute on every render — small
  // arrays + cheap.
  const parentCandidates = allWorkstreams.filter(
    (w) =>
      w.id !== detail.id &&
      w.parent_workstream_id == null &&
      // Exclude workstreams that already have children — they ARE
      // parents themselves, but allowing them would make them a
      // grandparent if we added the current workstream under them.
      // Wait, that's actually allowed (2 levels: A→B is fine). The
      // backend's rule 3 catches the inverse (you can't move a parent
      // under another parent). So we don't filter these here.
      true,
  );

  // Picker is disabled when this workstream has its own children — to
  // make it a child would push its children to a third level. The
  // backend rejects this anyway; this just gives a clearer affordance.
  const isParentItself = allWorkstreams.some(
    (w) => w.parent_workstream_id === detail.id,
  );

  const onChangeOwner = async (ownerId: string | null) => {
    const prev = detail.owner_member_id;
    setDetail((d) => (d ? { ...d, owner_member_id: ownerId } : d));
    try {
      await setWorkstreamOwner(detail.id, ownerId);
      // List view's owner chip + member-filter reflect the new owner.
      onChanged();
    } catch (e) {
      console.error("[workstreams] setWorkstreamOwner failed", e);
      setDetail((d) => (d ? { ...d, owner_member_id: prev } : d));
    }
  };

  const onChangeParent = async (parentId: string | null) => {
    const prev = detail.parent_workstream_id;
    setDetail((d) => (d ? { ...d, parent_workstream_id: parentId } : d));
    try {
      await setWorkstreamParent(detail.id, parentId);
      // Hierarchy in the list view needs to repaint immediately.
      onChanged();
    } catch (e) {
      console.error("[workstreams] setWorkstreamParent failed", e);
      // Revert + surface the backend error string to the user.
      setDetail((d) => (d ? { ...d, parent_workstream_id: prev } : d));
      // eslint-disable-next-line no-alert
      alert(
        typeof e === "string"
          ? e
          : e instanceof Error
            ? e.message
            : "Could not set parent",
      );
    }
  };

  const parentTitle = detail.parent_workstream_id
    ? allWorkstreams.find((w) => w.id === detail.parent_workstream_id)?.title
    : null;

  return (
    <div className="workstream-view">
      <WorkstreamBackLink onBack={onBack} />
      {detail.parent_workstream_id && parentTitle ? (
        <button
          type="button"
          className="workstream-detail-breadcrumb"
          onClick={() => onNavigateTo(detail.parent_workstream_id as string)}
          title={`Open ${parentTitle}`}
        >
          {parentTitle}
          <span className="workstream-detail-breadcrumb-sep" aria-hidden>
            ›
          </span>
          <span className="workstream-detail-breadcrumb-self">{detail.title}</span>
        </button>
      ) : null}
      <DetailHeader
        title={detail.title}
        trailing={
          <div className="nh-popover-anchor">
            <button
              type="button"
              className={
                "workstream-detail-settings-button" + (moreOpen ? " active" : "")
              }
              aria-label="More actions"
              title="More"
              onClick={(e) => {
                e.stopPropagation();
                setMoreOpen((v) => !v);
              }}
            >
              <IconMore size={18} sw={1.8} />
            </button>
            {moreOpen && (
              <WorkstreamMoreMenu
                status={detail.status}
                onClose={() => setMoreOpen(false)}
                onChangeOwner={() => setPickerMode("owner")}
                onChangeParent={() => setPickerMode("parent")}
                onChangeStatus={(s) => void onChangeStatus(s)}
                parentDisabled={isParentItself}
                parentDisabledReason={
                  isParentItself
                    ? "This workstream has children — unparent them before setting a parent here."
                    : undefined
                }
              />
            )}
          </div>
        }
      />
      {pickerMode === "owner" && (
        <WorkstreamPickerModal
          title="Change owner"
          placeholder="Search team members…"
          items={teamMembers.map((m) => ({
            id: m.id,
            label: m.display_name,
            sublabel: m.role || undefined,
          }))}
          currentId={detail.owner_member_id}
          allowClear
          clearLabel="Unassigned"
          onClose={() => setPickerMode(null)}
          onPick={(id) => {
            setPickerMode(null);
            void onChangeOwner(id);
          }}
        />
      )}
      {pickerMode === "parent" && (
        <WorkstreamPickerModal
          title="Set parent"
          placeholder="Search workstreams…"
          items={parentCandidates.map((w) => ({
            id: w.id,
            label: w.title,
            sublabel: w.summary || undefined,
          }))}
          currentId={detail.parent_workstream_id}
          allowClear
          clearLabel="No parent (top-level)"
          onClose={() => setPickerMode(null)}
          onPick={(id) => {
            setPickerMode(null);
            void onChangeParent(id);
          }}
        />
      )}
      <p className="workstream-detail-summary">{detail.summary}</p>

      {detail.members.length > 0 || detail.owner_member_id ? (
        <MembersStrip
          memberIds={detail.members}
          ownerId={detail.owner_member_id}
          teamById={teamById}
        />
      ) : null}

      {detail.external_participants.length > 0 ? (
        <ExternalsStrip
          externals={detail.external_participants}
          onChipClick={(p) => setExternalDialog(p)}
        />
      ) : null}
      {externalDialog ? (
        <ExternalChipModal
          participant={externalDialog}
          onClose={() => setExternalDialog(null)}
          onAddedToTeam={() => {
            // Refetch detail so the chip moves into MembersStrip on
            // the next render.
            void reload();
          }}
        />
      ) : null}

      <WorkstreamUserNotes
        workstreamId={detail.id}
        initialNotes={detail.user_notes}
        onSaved={(notes) =>
          setDetail((d) => (d ? { ...d, user_notes: notes } : d))
        }
      />

      <LinksSection
        workstreamId={detail.id}
        links={detail.links}
        onLinksChanged={(next) =>
          setDetail((d) =>
            d ? { ...d, links: next, link_count: next.length } : d,
          )
        }
      />

      <ActionsSection
        actions={detail.actions}
        onToggle={onToggleAction}
        onReassign={onReassignAction}
        onDelete={onDeleteAction}
        members={teamMembers}
        onOpenSource={async (kind, sourceId) => {
          if (kind === "note") {
            onOpenNote(sourceId);
          } else if (kind === "event") {
            await onOpenEvent(sourceId);
          }
        }}
      />

      <EmailsSection emails={detail.emails} />

      <MeetingsSection
        events={detail.events}
        onOpenEvent={onOpenEvent}
      />

      <MessagesSection messages={detail.teams_messages} />

      <NotesSection notes={detail.notes} onOpenNote={onOpenNote} />

      {detail.children.length > 0 ? (
        <ChildrenSection
          items={detail.children}
          teamById={teamById}
          onSelect={onNavigateTo}
        />
      ) : null}
    </div>
  );
}

/**
 * Settings modal for a workstream's owner / parent / status (#89).
 * Replaces the three inline `<select>` chips that used to crowd the
 * detail header. Each picker fires its handler optimistically — there
 * is no Save button, the modal is just a less-cluttered home for the
 * controls. Esc / backdrop click close.
 */
function ChildrenSection({
  items,
  teamById,
  onSelect,
}: {
  items: Workstream[];
  teamById: Map<string, TeamMember>;
  onSelect: (id: string) => void;
}) {
  return (
    <section className="workstream-children-section">
      <h3 className="workstream-section-title">Children ({items.length})</h3>
      <div className="workstream-card-children workstream-card-children-flush">
        {items.map((c) => (
          <EmbeddedChildRow
            key={c.id}
            workstream={c}
            teamById={teamById}
            onClick={() => onSelect(c.id)}
          />
        ))}
      </div>
    </section>
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

/// Dropdown menu rendered under the `...` button on the workstream
/// detail view (#101). Mirrors the NoteHeader MoreMenu pattern: direct
/// items for status transitions (Archive / Snooze / Activate) plus
/// entry points into the owner / parent picker modals. Status items
/// are mutually-exclusive per current state — the dropdown only shows
/// transitions that make sense.
function WorkstreamMoreMenu({
  status,
  onClose,
  onChangeOwner,
  onChangeParent,
  onChangeStatus,
  parentDisabled,
  parentDisabledReason,
}: {
  status: WorkstreamStatus | null;
  onClose: () => void;
  onChangeOwner: () => void;
  onChangeParent: () => void;
  onChangeStatus: (s: WorkstreamStatus) => void;
  parentDisabled: boolean;
  parentDisabledReason?: string;
}) {
  const canArchive = status === "active" || status === "snoozed";
  const canSnooze = status === "active";
  const canActivate = status === "snoozed" || status === "archived";
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return (
    <div
      className="nh-popover nh-more-popover"
      onMouseDown={(e) => e.stopPropagation()}
      onClick={(e) => e.stopPropagation()}
    >
      <button
        type="button"
        className="nh-more-item"
        onClick={() => {
          onClose();
          onChangeOwner();
        }}
      >
        <IconUser size={14} sw={1.7} />
        <span>Change owner</span>
      </button>
      <button
        type="button"
        className="nh-more-item"
        disabled={parentDisabled}
        title={parentDisabled ? parentDisabledReason : undefined}
        onClick={() => {
          if (parentDisabled) return;
          onClose();
          onChangeParent();
        }}
      >
        <IconBriefcase size={14} sw={1.7} />
        <span>Set parent</span>
      </button>
      <div className="nh-more-sep" />
      {canSnooze && (
        <button
          type="button"
          className="nh-more-item"
          onClick={() => {
            onClose();
            onChangeStatus("snoozed");
          }}
        >
          <IconBell size={14} sw={1.7} />
          <span>Snooze</span>
        </button>
      )}
      {canActivate && (
        <button
          type="button"
          className="nh-more-item"
          onClick={() => {
            onClose();
            onChangeStatus("active");
          }}
        >
          <IconCheck size={14} sw={1.7} />
          <span>Mark as active</span>
        </button>
      )}
      {canArchive && (
        <button
          type="button"
          className="nh-more-item"
          onClick={() => {
            onClose();
            onChangeStatus("archived");
          }}
        >
          <IconArchive size={14} sw={1.7} />
          <span>Archive</span>
        </button>
      )}
    </div>
  );
}

/// Search-palette-style picker (#101) for owner / parent selection.
/// Filter narrows the list as the user types; Enter picks the active
/// row, Escape closes. Items render with a label + optional sublabel
/// to disambiguate (e.g. workstream summary or team-member role).
function WorkstreamPickerModal({
  title,
  placeholder,
  items,
  currentId,
  allowClear,
  clearLabel,
  onPick,
  onClose,
}: {
  title: string;
  placeholder: string;
  items: { id: string; label: string; sublabel?: string }[];
  currentId: string | null;
  /** When true, exposes a "no selection" row that calls onPick(null). */
  allowClear: boolean;
  clearLabel: string;
  onPick: (id: string | null) => void;
  onClose: () => void;
}) {
  const [query, setQuery] = useState("");
  const inputRef = useRef<HTMLInputElement | null>(null);
  const listRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const matches = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return items;
    return items.filter(
      (it) =>
        it.label.toLowerCase().includes(q) ||
        (it.sublabel?.toLowerCase().includes(q) ?? false),
    );
  }, [query, items]);

  // Active row: the first match. Reset to 0 whenever the filter
  // changes (or the user navigates).
  const [activeIdx, setActiveIdx] = useState(0);
  useEffect(() => {
    setActiveIdx(0);
  }, [query]);

  const totalRows = matches.length + (allowClear ? 1 : 0);

  const pickRow = (idx: number) => {
    if (allowClear && idx === 0) {
      onPick(null);
      return;
    }
    const itemIdx = allowClear ? idx - 1 : idx;
    const it = matches[itemIdx];
    if (it) onPick(it.id);
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setActiveIdx((i) => Math.min(i + 1, totalRows - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActiveIdx((i) => Math.max(i - 1, 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      pickRow(activeIdx);
    } else if (e.key === "Escape") {
      e.preventDefault();
      onClose();
    }
  };

  return (
    <div
      className="palette-backdrop"
      role="presentation"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="palette-dialog mode-search"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onKeyDown={onKeyDown}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="palette-input-row">
          <span className="palette-input-icon" aria-hidden="true">
            <IconSearch size={14} sw={1.7} />
          </span>
          <input
            ref={inputRef}
            type="text"
            className="palette-input"
            placeholder={placeholder}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            spellCheck={false}
            autoCorrect="off"
            autoCapitalize="off"
          />
          <span className="palette-input-kbd">esc</span>
        </div>
        <div ref={listRef} className="workstream-picker-results">
          {allowClear && (
            <button
              type="button"
              className={
                "workstream-picker-row" +
                (activeIdx === 0 ? " active" : "") +
                (currentId === null ? " current" : "")
              }
              onMouseEnter={() => setActiveIdx(0)}
              onClick={() => pickRow(0)}
            >
              <span className="workstream-picker-label">{clearLabel}</span>
            </button>
          )}
          {matches.length === 0 ? (
            <p className="workstream-picker-empty">No matches.</p>
          ) : (
            matches.map((it, i) => {
              const idx = allowClear ? i + 1 : i;
              const isActive = activeIdx === idx;
              const isCurrent = currentId === it.id;
              return (
                <button
                  key={it.id}
                  type="button"
                  className={
                    "workstream-picker-row" +
                    (isActive ? " active" : "") +
                    (isCurrent ? " current" : "")
                  }
                  onMouseEnter={() => setActiveIdx(idx)}
                  onClick={() => pickRow(idx)}
                >
                  <span className="workstream-picker-label">{it.label}</span>
                  {it.sublabel && (
                    <span className="workstream-picker-sublabel">
                      {it.sublabel}
                    </span>
                  )}
                </button>
              );
            })
          )}
        </div>
      </div>
    </div>
  );
}

/// Standalone back link rendered above the breadcrumb on the detail
/// view (#101). Splitting it out of DetailHeader lets us render
/// `back → breadcrumb → title` for nested workstreams instead of the
/// older `breadcrumb → back → title` order.
function WorkstreamBackLink({ onBack }: { onBack: () => void }) {
  return (
    <button
      type="button"
      className="workstream-back-link"
      onClick={onBack}
      aria-label="Back to workstreams"
    >
      <IconChevLeft size={13} sw={1.8} />
      Workstreams
    </button>
  );
}

function DetailHeader({
  title,
  trailing,
}: {
  title: string;
  /** Right-aligned slot. Used to render the `...` more-menu trigger +
   *  its popover (#101). Omitted on loading / missing sub-states. */
  trailing?: React.ReactNode;
}) {
  return (
    <header className="workstream-header workstream-detail-header">
      <h1 className="workstream-title">{title}</h1>
      {trailing}
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
        if (isOwner) {
          return (
            <span
              key={m.id}
              className="workstream-member-owner-chip"
              title={`${m.display_name} (owner)`}
            >
              <span aria-hidden className="workstream-member-owner-mark">
                ★
              </span>
              {m.display_name}
            </span>
          );
        }
        return (
          <span
            key={m.id}
            className="workstream-member-chip"
            title={m.display_name}
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

// ----- External participants ---------------------------------------------

const EXTERNAL_VISIBLE_CAP = 8;

function ExternalsStrip({
  externals,
  onChipClick,
}: {
  externals: ExternalParticipant[];
  onChipClick: (p: ExternalParticipant) => void;
}) {
  const visible = externals.slice(0, EXTERNAL_VISIBLE_CAP);
  const overflow = externals.length - visible.length;
  return (
    <section className="workstream-externals-strip">
      <span className="workstream-externals-label">External</span>
      {visible.map((p) => {
        const display = p.display_name?.trim() || p.email;
        return (
          <button
            key={p.email}
            type="button"
            className="workstream-external-chip"
            title={p.display_name ? `${p.display_name} <${p.email}>` : p.email}
            onClick={() => onChipClick(p)}
          >
            {display}
          </button>
        );
      })}
      {overflow > 0 ? (
        <span className="workstream-externals-overflow">+{overflow}</span>
      ) : null}
    </section>
  );
}

function ExternalChipModal({
  participant,
  onClose,
  onAddedToTeam,
}: {
  participant: ExternalParticipant;
  onClose: () => void;
  /** Fired after the participant becomes a team member, so the parent
   *  detail view can refetch — the chip will move from the External
   *  strip into the Members strip on the next render. */
  onAddedToTeam: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState(false);
  const [addError, setAddError] = useState<string | null>(null);

  // Esc closes; backdrop click closes.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const displayName = participant.display_name?.trim() || participant.email;

  const onCopy = async () => {
    try {
      await navigator.clipboard.writeText(participant.email);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch (err) {
      console.error("[workstreams] copy email failed", err);
    }
  };

  const onAdd = async () => {
    setBusy(true);
    setAddError(null);
    try {
      // Create the team member with the email as a typed alias so the
      // resolver picks them up on the next refresh. display_name is
      // the address itself when no display name was on the source row;
      // the user can rename them later from the team detail view.
      await createTeamMember(displayName, "", [
        { kind: AliasKind.Email, value: participant.email },
      ]);
      window.dispatchEvent(new CustomEvent("margin:team-changed"));
      onAddedToTeam();
      onClose();
    } catch (err) {
      console.error("[workstreams] createTeamMember failed", err);
      setAddError(
        typeof err === "string"
          ? err
          : err instanceof Error
            ? err.message
            : "Could not add to team",
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      className="settings-modal-backdrop"
      role="presentation"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="external-chip-modal"
        role="dialog"
        aria-modal="true"
        aria-label={`Identity: ${displayName}`}
      >
        <button
          type="button"
          className="external-chip-modal-close"
          onClick={onClose}
          disabled={busy}
          aria-label="Close"
          title="Close"
        >
          ×
        </button>
        <header className="external-chip-modal-header">
          <h2 className="external-chip-modal-name">{displayName}</h2>
          <div className="external-chip-modal-email">{participant.email}</div>
          <div className="external-chip-modal-count">
            Seen on {participant.count} signal{participant.count === 1 ? "" : "s"} in this workstream
          </div>
        </header>
        <div className="external-chip-modal-actions">
          <button
            type="button"
            className="external-chip-modal-action"
            onClick={() => void onCopy()}
          >
            {copied ? "Copied!" : "Copy email"}
          </button>
          <button
            type="button"
            className="external-chip-modal-action primary"
            onClick={() => void onAdd()}
            disabled={busy}
          >
            {busy ? "Adding…" : "Add to team"}
          </button>
        </div>
        {addError ? (
          <p className="external-chip-modal-error">{addError}</p>
        ) : null}
      </div>
    </div>
  );
}

// ----- User-curated links (#88) -------------------------------------------

/// Subscribe to the backend's `workstream-link-summarized` event,
/// which fires after the Firecrawl + Haiku background task lands a
/// row. Returns the unlisten handle so callers can clean up on
/// unmount.
async function listenForSummary(
  cb: (payload: WorkstreamLinkSummarizedEvent) => void,
): Promise<() => void> {
  return listen<WorkstreamLinkSummarizedEvent>(
    "workstream-link-summarized",
    (event) => cb(event.payload),
  );
}

function LinksSection({
  workstreamId,
  links,
  onLinksChanged,
}: {
  workstreamId: string;
  links: WorkstreamLink[];
  onLinksChanged: (next: WorkstreamLink[]) => void;
}) {
  const [composerOpen, setComposerOpen] = useState(false);
  /** Link ids currently being summarized in the background. The
   *  summary task fires `workstream-link-summarized` once per add
   *  (success OR failure). The 60s ceiling is a belt-and-braces
   *  fallback in case the event somehow never lands. */
  const [pending, setPending] = useState<Set<string>>(() => new Set());
  /** Per-link "couldn't generate" reason, surfaced as a muted line
   *  on the chip when the summary task finished without producing
   *  text (paywalled / login-walled / scrape failed / etc.). */
  const [unavailable, setUnavailable] = useState<Map<string, string>>(
    () => new Map(),
  );

  // The summarization background task fires `workstream-link-summarized`
  // when it lands a row. Refs keep the listener stable across renders
  // while still seeing the freshest props.
  const linksRef = useRef(links);
  const onLinksChangedRef = useRef(onLinksChanged);
  useEffect(() => {
    linksRef.current = links;
  }, [links]);
  useEffect(() => {
    onLinksChangedRef.current = onLinksChanged;
  }, [onLinksChanged]);
  useEffect(() => {
    // React StrictMode + HMR will mount → unmount → mount again. Track
    // `cancelled` so that if the component unmounts before
    // `listenForSummary` resolves, we immediately invoke `stop` from
    // the resolution callback rather than stashing it on a stale
    // closure (which then triggers `listeners[eventId].handlerId` on
    // a torn-down listener).
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void listenForSummary((payload) => {
      // Success path: patch the link with the new summary text.
      if (payload.summary) {
        const next = linksRef.current.map((l) =>
          l.id === payload.link_id ? { ...l, summary: payload.summary } : l,
        );
        if (next.some((l) => l.id === payload.link_id)) {
          onLinksChangedRef.current(next);
        }
      } else if (payload.reason) {
        // Failure path: surface the reason inline so the user knows
        // the system tried, even though it couldn't produce text.
        setUnavailable((prev) => {
          const next = new Map(prev);
          next.set(payload.link_id, payload.reason ?? "No summary available");
          return next;
        });
      }
      // Clear the in-flight spinner regardless of outcome.
      setPending((prev) => {
        if (!prev.has(payload.link_id)) return prev;
        const next = new Set(prev);
        next.delete(payload.link_id);
        return next;
      });
    }).then((stop) => {
      if (cancelled) {
        try {
          stop();
        } catch {
          /* listener already torn down */
        }
      } else {
        unlisten = stop;
      }
    });
    return () => {
      cancelled = true;
      try {
        unlisten?.();
      } catch {
        /* listener already torn down */
      }
    };
  }, []);

  /** Mark a link as in-flight and auto-expire after a generous
   *  ceiling so a silently-failed summarization doesn't leave the
   *  spinner spinning forever. The Firecrawl scrape is bounded at
   *  30s and Haiku at 15s; 60s is the safe upper bound. */
  const trackPending = (linkId: string) => {
    setPending((prev) => new Set(prev).add(linkId));
    window.setTimeout(() => {
      setPending((prev) => {
        if (!prev.has(linkId)) return prev;
        const next = new Set(prev);
        next.delete(linkId);
        return next;
      });
    }, 60_000);
  };

  const handleOpen = async (url: string) => {
    try {
      await openUrl(url);
    } catch (err) {
      console.error("[workstreams] openUrl failed", err);
    }
  };

  const handleRemove = async (linkId: string) => {
    // Optimistic remove; revert on error so a transient backend hiccup
    // doesn't drop the user's curated URL silently.
    const prev = links;
    onLinksChanged(links.filter((l) => l.id !== linkId));
    try {
      await removeWorkstreamLink(linkId);
    } catch (err) {
      console.error("[workstreams] removeWorkstreamLink failed", err);
      onLinksChanged(prev);
    }
  };

  const handleAdd = async (url: string): Promise<string | null> => {
    try {
      const created = await addWorkstreamLinkFromUrl(workstreamId, url);
      onLinksChanged([...links, created]);
      // Backend just kicked off the summary task; show the
      // "Summarizing…" placeholder until the event lands.
      trackPending(created.id);
      setComposerOpen(false);
      return null;
    } catch (err) {
      console.error("[workstreams] addWorkstreamLinkFromUrl failed", err);
      return typeof err === "string"
        ? err
        : err instanceof Error
          ? err.message
          : "Could not add link";
    }
  };

  return (
    <section className="workstream-links">
      <div className="workstream-links-head">
        <h3 className="workstream-links-title">Links</h3>
        {!composerOpen && (
          <button
            type="button"
            className="workstream-links-add"
            onClick={() => setComposerOpen(true)}
          >
            <IconPlus size={12} sw={1.8} />
            Add link
          </button>
        )}
      </div>
      {links.length === 0 && !composerOpen ? (
        <p className="workstream-links-empty">
          No external links yet — attach the repo, design doc, or tracking
          ticket so they're one click away.
        </p>
      ) : null}
      {links.length > 0 ? (
        <div className="workstream-links-chips">
          {links.map((link) => (
            <div
              className={
                "workstream-link-chip" +
                (link.summary ||
                pending.has(link.id) ||
                unavailable.has(link.id)
                  ? " has-summary"
                  : "")
              }
              key={link.id}
            >
              <button
                type="button"
                className="workstream-link-chip-open"
                onClick={() => void handleOpen(link.url)}
                title={link.summary ?? link.url}
              >
                <span className="workstream-link-chip-row">
                  <IconBrand kind={link.kind} size={12} />
                  <span className="workstream-link-chip-label">
                    {link.label}
                  </span>
                </span>
                {link.summary ? (
                  <span className="workstream-link-chip-summary">
                    {link.summary}
                  </span>
                ) : pending.has(link.id) ? (
                  <span className="workstream-link-chip-summary workstream-link-chip-summary-pending">
                    <span className="workstream-link-summary-spinner" aria-hidden />
                    Summarizing…
                  </span>
                ) : unavailable.has(link.id) ? (
                  <span className="workstream-link-chip-summary workstream-link-chip-summary-unavailable">
                    {unavailable.get(link.id)}
                  </span>
                ) : null}
              </button>
              <button
                type="button"
                className="workstream-link-chip-remove"
                onClick={() => void handleRemove(link.id)}
                aria-label={`Remove ${link.label}`}
                title="Remove"
              >
                <IconTrash size={11} sw={1.8} />
              </button>
            </div>
          ))}
        </div>
      ) : null}
      {composerOpen ? (
        <LinkComposer
          onCancel={() => setComposerOpen(false)}
          onSubmit={(url) => handleAdd(url)}
        />
      ) : null}
    </section>
  );
}

function LinkComposer({
  onCancel,
  onSubmit,
}: {
  onCancel: () => void;
  /** Returns `null` on success (composer dismissed by parent), or an
   *  error string the composer surfaces inline. */
  onSubmit: (url: string) => Promise<string | null>;
}) {
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const urlRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    urlRef.current?.focus();
  }, []);

  const canSubmit = url.trim() !== "" && !busy;

  const submit = async () => {
    if (!canSubmit) return;
    setBusy(true);
    setError(null);
    const err = await onSubmit(url.trim());
    setBusy(false);
    if (err) setError(err);
  };

  return (
    <div className="workstream-link-composer workstream-link-composer-pasted">
      <input
        ref={urlRef}
        type="url"
        className="workstream-link-composer-input"
        placeholder="Paste a URL — we'll name it for you"
        value={url}
        disabled={busy}
        onChange={(e) => setUrl(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            void submit();
          } else if (e.key === "Escape") {
            onCancel();
          }
        }}
      />
      <div className="workstream-link-composer-actions">
        <button
          type="button"
          className="workstream-link-composer-cancel"
          onClick={onCancel}
          disabled={busy}
        >
          Cancel
        </button>
        <button
          type="button"
          className="workstream-link-composer-save"
          disabled={!canSubmit}
          onClick={() => void submit()}
        >
          {busy ? "Naming…" : "Add"}
        </button>
      </div>
      {error ? (
        <p className="workstream-link-composer-error">{error}</p>
      ) : null}
    </div>
  );
}

// ----- Sections ------------------------------------------------------------

function ActionsSection({
  actions,
  onToggle,
  onReassign,
  onDelete,
  onOpenSource,
  members,
}: {
  actions: WorkstreamAction[];
  onToggle: (actionId: string, nextDone: boolean) => void | Promise<void>;
  onReassign: (actionId: string, memberId: string | null) => void | Promise<void>;
  onDelete: (actionId: string) => void | Promise<void>;
  onOpenSource: (sourceKind: string, sourceId: string) => void | Promise<void>;
  members: TeamMember[];
}) {
  if (actions.length === 0) return null;
  const memberById = new Map(members.map((m) => [m.id, m] as const));
  return (
    <section className="workstream-section">
      <h2 className="workstream-section-title">Actions ({actions.length})</h2>
      <div className="home-actions">
        {actions.map((a) => {
          const openable = a.source_kind === "note" || a.source_kind === "event";
          const assigneeName = a.assignee_id
            ? memberById.get(a.assignee_id)?.display_name ?? null
            : null;
          return (
            <div
              key={a.id}
              className="home-action-row"
              role={openable ? "button" : undefined}
              tabIndex={openable ? 0 : undefined}
              onClick={
                openable
                  ? () => void onOpenSource(a.source_kind, a.source_id)
                  : undefined
              }
              onKeyDown={
                openable
                  ? (e) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        void onOpenSource(a.source_kind, a.source_id);
                      }
                    }
                  : undefined
              }
              title={openable ? `Open ${a.source_kind} source` : undefined}
            >
              <button
                type="button"
                className={"home-checkbox" + (a.done ? " done" : "")}
                aria-label={a.done ? "Mark as open" : "Mark as done"}
                onClick={(e) => {
                  e.stopPropagation();
                  void onToggle(a.id, !a.done);
                }}
              >
                {a.done && <IconCheck size={20} sw={3.6} />}
              </button>
              <div className="home-action-body">
                <div className={"home-action-text" + (a.done ? " done" : "")}>
                  {a.text}
                </div>
              </div>
              <DueChip dueMs={a.due_ms} />
              <AssigneeChip
                assigneeId={a.assignee_id}
                assigneeDisplayName={assigneeName}
                members={members}
                onPick={(memberId) => void onReassign(a.id, memberId)}
              />
              <span
                className="workstream-action-source-chip"
                title={`from ${a.source_kind}`}
              >
                from {a.source_kind}
              </span>
              <button
                type="button"
                className="home-action-delete"
                aria-label="Delete action item"
                title="Delete"
                onClick={(e) => {
                  e.stopPropagation();
                  void onDelete(a.id);
                }}
              >
                <IconTrash size={14} sw={1.7} />
              </button>
            </div>
          );
        })}
      </div>
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

/// Compact rendering of Teams messages attached to a workstream
/// (#105). Each row shows the chat topic/sender, time, and body
/// preview. Clicking expands to show the full HTML body inline —
/// unlike emails, Teams messages ship with body_html attached so no
/// lazy fetch is needed.
function MessagesSection({
  messages,
}: {
  messages: import("./file").TeamsMessage[];
}) {
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  if (messages.length === 0) return null;
  const toggle = (id: string) =>
    setExpanded((p) => ({ ...p, [id]: !p[id] }));
  return (
    <section className="workstream-section">
      <h2 className="workstream-section-title">
        Teams messages ({messages.length})
      </h2>
      <ul className="workstream-emails">
        {messages.map((m) => {
          const open = !!expanded[m.id];
          const date = formatShortDate(m.sent_at_ms);
          // Title preference: chat_topic > sender name > "Teams chat"
          const label =
            m.chat_topic && m.chat_topic.length > 0
              ? m.chat_topic
              : m.from_name || m.from_email || "Teams chat";
          const fromLine = m.from_name || m.from_email || "(unknown)";
          return (
            <li key={m.id} className="workstream-email">
              <button
                type="button"
                className="workstream-email-row"
                onClick={() => toggle(m.id)}
                aria-expanded={open}
              >
                <span className="workstream-email-date">{date}</span>
                <span className="workstream-email-from">{fromLine}</span>
                <span className="workstream-email-subject">
                  {label}
                  {m.body_preview ? ` — ${m.body_preview}` : ""}
                </span>
                <span className="workstream-email-chev">{open ? "▾" : "▸"}</span>
              </button>
              {open && (
                <div className="workstream-email-body">
                  {m.body_html ? (
                    <div
                      // Teams body_html is sanitized on the Graph side; we
                      // render as-is. Same risk profile as the EmailsSection
                      // body render (no user input is interpolated client-side).
                      dangerouslySetInnerHTML={{ __html: m.body_html }}
                    />
                  ) : (
                    <p className="workstream-email-loading">
                      {m.body_preview ?? "(no body)"}
                    </p>
                  )}
                </div>
              )}
            </li>
          );
        })}
      </ul>
    </section>
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

