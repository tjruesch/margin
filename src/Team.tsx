import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ask } from "@tauri-apps/plugin-dialog";

import { dueBucket } from "./dueLabel";
import { ActionRow, BUCKET_ORDER } from "./Home";
import { IconChevLeft, IconPlus, IconTrash } from "./icons";
import {
  AliasKind,
  type ActionListItem,
  type ProfileObservation,
  type ProfileSnapshot,
  type TeamMember,
  type TypedAlias,
  acceptProfileObservation,
  createTeamMember,
  deleteTeamMember,
  forceRecomputeProfile,
  getProfileSnapshot,
  listActions,
  listProfileObservations,
  listTeamMembers,
  pendingObservationCounts,
  rejectProfileObservation,
  updateTeamMember,
} from "./file";
import { avatarColor, initialsFromName } from "./initials";

export function TeamView({
  onOpenNote,
  onOpenWorkstream,
  onToggleAction,
  onReassignAction,
}: {
  onOpenNote: (path: string) => void;
  /** Routes workstream-sourced rows to the Workstreams view (#100). */
  onOpenWorkstream: (id: string) => void;
  onToggleAction: (id: string, nextDone: boolean) => void;
  onReassignAction: (actionId: string, memberId: string | null) => Promise<void>;
}) {
  const [members, setMembers] = useState<TeamMember[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [pendingCounts, setPendingCounts] = useState<Record<string, number>>({});

  const reload = useCallback(async () => {
    const fresh = await listTeamMembers();
    setMembers(fresh);
  }, []);

  const reloadCounts = useCallback(async () => {
    try {
      setPendingCounts(await pendingObservationCounts());
    } catch (err) {
      console.error("pendingObservationCounts failed:", err);
    }
  }, []);

  useEffect(() => {
    void reloadCounts();
  }, [reloadCounts]);

  // Tell App.tsx (and anyone else holding a member-list copy) that the
  // roster changed. Dispatched after any create / update / delete so
  // the assignee-chip dropdown on action rows (#51) sees the new list
  // without forcing a full app reload.
  const announceTeamChanged = useCallback(() => {
    window.dispatchEvent(new CustomEvent("margin:team-changed"));
  }, []);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const fresh = await listTeamMembers();
        if (!cancelled) setMembers(fresh);
      } catch (err) {
        console.error("listTeamMembers failed:", err);
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  if (loading) {
    return <div className="team-loading" />;
  }

  if (selectedId) {
    const member = members.find((m) => m.id === selectedId);
    if (!member) {
      // Member was deleted out from under us; bounce back.
      setSelectedId(null);
      return null;
    }
    return (
      <TeamDetail
        member={member}
        members={members}
        pendingCount={pendingCounts[member.id] ?? 0}
        onBack={() => setSelectedId(null)}
        onSelectMember={setSelectedId}
        onOpenNote={onOpenNote}
        onOpenWorkstream={onOpenWorkstream}
        onToggleAction={onToggleAction}
        onReassignAction={onReassignAction}
        onObservationsChanged={() => void reloadCounts()}
        onUpdated={(next) => {
          setMembers((prev) => prev.map((m) => (m.id === next.id ? next : m)));
          announceTeamChanged();
        }}
        onDeleted={() => {
          setMembers((prev) => prev.filter((m) => m.id !== member.id));
          setSelectedId(null);
          announceTeamChanged();
        }}
      />
    );
  }

  return (
    <ListPane
      members={members}
      pendingCounts={pendingCounts}
      onSelect={setSelectedId}
      onCreated={async (m) => {
        await reload();
        setSelectedId(m.id);
        announceTeamChanged();
      }}
    />
  );
}

// ---------- List pane ----------------------------------------------------

function ListPane({
  members,
  pendingCounts,
  onSelect,
  onCreated,
}: {
  members: TeamMember[];
  pendingCounts: Record<string, number>;
  onSelect: (id: string) => void;
  onCreated: (member: TeamMember) => void;
}) {
  const [composerOpen, setComposerOpen] = useState(false);
  // The "Add team member" trigger lives in PageHeader (Home.tsx) when
  // nav === "team"; we listen for its dispatched event and open the
  // inline composer below.
  useEffect(() => {
    const onOpen = () => setComposerOpen(true);
    window.addEventListener("margin:open-team-composer", onOpen);
    return () =>
      window.removeEventListener("margin:open-team-composer", onOpen);
  }, []);
  return (
    <section className="home-section">
      {composerOpen && (
        <TeamComposerForm
          onClose={() => setComposerOpen(false)}
          onCreated={(m) => {
            setComposerOpen(false);
            onCreated(m);
          }}
        />
      )}
      {members.length === 0 ? (
        <p className="home-empty">
          No team members yet — start with someone you work with regularly so
          Claude can attribute action items to them by name.
        </p>
      ) : (
        <div className="team-list">
          {members.map((m) => (
            <TeamRow
              key={m.id}
              member={m}
              pendingCount={pendingCounts[m.id] ?? 0}
              onClick={() => onSelect(m.id)}
            />
          ))}
        </div>
      )}
    </section>
  );
}

function TeamRow({
  member,
  pendingCount,
  onClick,
}: {
  member: TeamMember;
  pendingCount: number;
  onClick: () => void;
}) {
  return (
    <div
      className="team-row"
      role="button"
      tabIndex={0}
      onClick={onClick}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onClick();
        }
      }}
    >
      <Avatar member={member} size={38} />
      <div className="team-row-body">
        <div className="team-row-name">
          {member.display_name}
          {member.is_self && <span className="team-self-badge">You</span>}
        </div>
        <div className="team-row-role">
          {member.role || (member.is_self ? "Your profile" : "—")}
        </div>
      </div>
      {pendingCount > 0 && (
        <span
          className="team-row-pending"
          title={`${pendingCount} suggestion${pendingCount === 1 ? "" : "s"} pending`}
        >
          {pendingCount}
        </span>
      )}
    </div>
  );
}

function Avatar({ member, size }: { member: TeamMember; size: number }) {
  const initials = initialsFromName(member.display_name);
  return (
    <span
      className="team-avatar"
      style={{
        background: avatarColor(member.id),
        width: size,
        height: size,
        fontSize: Math.round(size * 0.36),
      }}
    >
      {initials}
    </span>
  );
}

function TeamComposerForm({
  onClose,
  onCreated,
}: {
  onClose: () => void;
  onCreated: (member: TeamMember) => void;
}) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const inputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const submit = async () => {
    const trimmed = text.trim();
    if (!trimmed || busy) return;
    setBusy(true);
    try {
      const member = await createTeamMember(trimmed, "", []);
      setText("");
      onCreated(member);
    } catch (err) {
      console.error("createTeamMember failed:", err);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="team-composer">
      <input
        ref={inputRef}
        type="text"
        className="team-composer-text"
        placeholder="Their name"
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            void submit();
          } else if (e.key === "Escape") {
            setText("");
            onClose();
          }
        }}
      />
      <div className="team-composer-actions">
        <button
          type="button"
          className="inbox-composer-cancel"
          onClick={() => {
            setText("");
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
          Add
        </button>
      </div>
    </div>
  );
}

// ---------- Detail pane --------------------------------------------------

function TeamDetail({
  member,
  members,
  pendingCount,
  onBack,
  onSelectMember,
  onUpdated,
  onDeleted,
  onOpenNote,
  onOpenWorkstream,
  onToggleAction,
  onReassignAction,
  onObservationsChanged,
}: {
  member: TeamMember;
  members: TeamMember[];
  pendingCount: number;
  onBack: () => void;
  onSelectMember: (id: string) => void;
  onUpdated: (next: TeamMember) => void;
  onDeleted: () => void;
  onOpenNote: (path: string) => void;
  onOpenWorkstream: (id: string) => void;
  onToggleAction: (id: string, nextDone: boolean) => void;
  onReassignAction: (actionId: string, memberId: string | null) => Promise<void>;
  onObservationsChanged: () => void;
}) {
  const [tab, setTab] = useState<"profile" | "suggestions" | "tasks">("profile");
  // Cross-link state (#115). A click on a citation chip in the Profile
  // tab sets `highlightObsId` and bumps `flashKey`; the SuggestionsTab
  // effect depends on both so re-clicks of the same id re-trigger the
  // scroll-and-flash sequence.
  const [highlightObsId, setHighlightObsId] = useState<string | null>(null);
  const [flashKey, setFlashKey] = useState(0);

  const onCiteClick = useCallback((obsId: string) => {
    setTab("suggestions");
    setHighlightObsId(obsId);
    setFlashKey((k) => k + 1);
  }, []);

  // Reset to Profile when switching to a different member so the user
  // never lands on a stale Tasks tab from the previous selection.
  useEffect(() => {
    setTab("profile");
    setHighlightObsId(null);
  }, [member.id]);

  const commitName = async (next: string) => {
    const trimmed = next.trim();
    if (!trimmed || trimmed === member.display_name) return;
    try {
      const updated = await updateTeamMember(member.id, { displayName: trimmed });
      onUpdated(updated);
    } catch (err) {
      console.error("updateTeamMember (name) failed:", err);
    }
  };

  const commitRole = async (next: string) => {
    const trimmed = next.trim();
    if (trimmed === member.role) return;
    try {
      const updated = await updateTeamMember(member.id, { role: trimmed });
      onUpdated(updated);
    } catch (err) {
      console.error("updateTeamMember (role) failed:", err);
    }
  };

  const [showIdentitiesModal, setShowIdentitiesModal] = useState(false);

  const saveIdentities = async (next: TypedAlias[]) => {
    try {
      const updated = await updateTeamMember(member.id, { aliases: next });
      onUpdated(updated);
    } catch (err) {
      console.error("updateTeamMember (aliases) failed:", err);
    }
  };

  const onDelete = async () => {
    const ok = await ask(
      `Delete ${member.display_name}? Their profile and any meeting attendance records are removed. Action items already assigned to them stay, but become unassigned.`,
      {
        title: "Delete team member?",
        kind: "warning",
        okLabel: "Delete",
        cancelLabel: "Cancel",
      },
    );
    if (!ok) return;
    try {
      await deleteTeamMember(member.id);
      onDeleted();
    } catch (err) {
      console.error("deleteTeamMember failed:", err);
    }
  };

  return (
    <section className="team-detail">
      <div className="team-detail-toolbar">
        <button
          type="button"
          className="team-detail-back"
          onClick={onBack}
        >
          <IconChevLeft size={14} sw={1.8} />
          Back to team
        </button>
        {!member.is_self && (
          <button
            type="button"
            className="team-detail-delete"
            onClick={() => void onDelete()}
          >
            <IconTrash size={13} sw={1.8} />
            Delete
          </button>
        )}
      </div>
      <header className="team-detail-header">
        <Avatar member={member} size={64} />
        <div className="team-detail-fields">
          <EditableField
            value={member.display_name}
            placeholder="Name"
            onCommit={(v) => void commitName(v)}
            className="team-detail-name"
            blankFallback={member.display_name}
          />
          <EditableField
            value={member.role}
            placeholder="Role (e.g. SDK lead)"
            onCommit={(v) => void commitRole(v)}
            className="team-detail-role"
          />
          <button
            type="button"
            className="team-detail-aliases-trigger"
            onClick={() => setShowIdentitiesModal(true)}
          >
            {identitiesSummary(member.aliases)}
          </button>
        </div>
      </header>
      {showIdentitiesModal && (
        <IdentitiesModal
          aliases={member.aliases}
          onClose={() => setShowIdentitiesModal(false)}
          onSave={async (next) => {
            await saveIdentities(next);
            setShowIdentitiesModal(false);
          }}
        />
      )}
      <div className="team-detail-tabs">
        <div className="nh-segmented" role="tablist" aria-label="Section">
          <button
            type="button"
            role="tab"
            aria-selected={tab === "profile"}
            className={"nh-segmented-btn" + (tab === "profile" ? " active" : "")}
            onClick={() => setTab("profile")}
          >
            Profile
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={tab === "suggestions"}
            className={
              "nh-segmented-btn" + (tab === "suggestions" ? " active" : "")
            }
            onClick={() => setTab("suggestions")}
          >
            Suggestions
            {pendingCount > 0 && (
              <span className="team-tab-badge">{pendingCount}</span>
            )}
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={tab === "tasks"}
            className={"nh-segmented-btn" + (tab === "tasks" ? " active" : "")}
            onClick={() => setTab("tasks")}
          >
            Tasks
          </button>
        </div>
      </div>
      {tab === "profile" && (
        <ProfileSnapshotPane
          member={member}
          members={members}
          onSelectMember={onSelectMember}
          onOpenWorkstream={onOpenWorkstream}
          onCiteClick={onCiteClick}
        />
      )}
      {tab === "suggestions" && (
        <SuggestionsTab
          member={member}
          onOpenNote={onOpenNote}
          onChanged={onObservationsChanged}
          highlightId={highlightObsId}
          flashKey={flashKey}
        />
      )}
      {tab === "tasks" && (
        <TasksTab
          member={member}
          members={members}
          onOpenNote={onOpenNote}
          onOpenWorkstream={onOpenWorkstream}
          onToggleAction={onToggleAction}
          onReassignAction={onReassignAction}
        />
      )}
    </section>
  );
}

// ---------- Profile snapshot pane (#107) --------------------------------

function ProfileSnapshotPane({
  member,
  members,
  onSelectMember,
  onOpenWorkstream,
  onCiteClick,
}: {
  member: TeamMember;
  members: TeamMember[];
  onSelectMember: (id: string) => void;
  onOpenWorkstream: (id: string) => void;
  onCiteClick: (obsId: string) => void;
}) {
  const [snap, setSnap] = useState<ProfileSnapshot | null | "loading">(
    "loading",
  );
  const [acceptedById, setAcceptedById] = useState<
    Map<string, ProfileObservation>
  >(() => new Map());
  const [recomputing, setRecomputing] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setSnap("loading");
    setAcceptedById(new Map());
    void (async () => {
      try {
        const [s, accepted] = await Promise.all([
          getProfileSnapshot(member.id),
          listProfileObservations(member.id, "accepted"),
        ]);
        if (cancelled) return;
        setSnap(s);
        const map = new Map<string, ProfileObservation>();
        for (const obs of accepted) map.set(obs.id, obs);
        setAcceptedById(map);
      } catch (err) {
        console.error("ProfileSnapshotPane fetch failed:", err);
        if (!cancelled) setSnap(null);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [member.id]);

  const onRefresh = async () => {
    setRecomputing(true);
    try {
      const [fresh, accepted] = await Promise.all([
        forceRecomputeProfile(member.id),
        listProfileObservations(member.id, "accepted"),
      ]);
      setSnap(fresh);
      const map = new Map<string, ProfileObservation>();
      for (const obs of accepted) map.set(obs.id, obs);
      setAcceptedById(map);
    } catch (err) {
      console.error("forceRecomputeProfile failed:", err);
    } finally {
      setRecomputing(false);
    }
  };

  if (snap === "loading") return <div className="team-tasks-loading" />;
  if (snap === null) {
    return (
      <div className="team-profile-empty">
        <p className="home-empty">
          Snapshot not yet computed. Margin builds this from your activity
          with {member.display_name}; it'll appear within an hour.
        </p>
        <button
          type="button"
          className="settings-btn"
          onClick={() => void onRefresh()}
          disabled={recomputing}
        >
          {recomputing ? "Computing…" : "Compute now"}
        </button>
      </div>
    );
  }
  return (
    <ProfileSnapshotView
      snap={snap}
      members={members}
      acceptedById={acceptedById}
      onSelectMember={onSelectMember}
      onOpenWorkstream={onOpenWorkstream}
      onCiteClick={onCiteClick}
      onRefresh={() => void onRefresh()}
      refreshing={recomputing}
    />
  );
}

function ProfileSnapshotView({
  snap,
  members,
  acceptedById,
  onSelectMember,
  onOpenWorkstream,
  onCiteClick,
  onRefresh,
  refreshing,
}: {
  snap: ProfileSnapshot;
  members: TeamMember[];
  acceptedById: Map<string, ProfileObservation>;
  onSelectMember: (id: string) => void;
  onOpenWorkstream: (id: string) => void;
  onCiteClick: (obsId: string) => void;
  onRefresh: () => void;
  refreshing: boolean;
}) {
  const memberById = useMemo(() => {
    const map = new Map<string, TeamMember>();
    for (const m of members) map.set(m.id, m);
    return map;
  }, [members]);

  const { body, computed_ms } = snap;
  const collaborators = body.frequent_collaborators ?? [];
  const focus = body.recent_focus ?? [];
  // Resolve cited observation ids to their full ProfileObservation rows;
  // silently skip ids that no longer match an accepted observation
  // (rejected/deleted since the snapshot was computed — stale cite).
  const citedObservations = useMemo(
    () =>
      (body.evidence_observation_ids ?? [])
        .map((id) => acceptedById.get(id))
        .filter((o): o is ProfileObservation => o !== undefined),
    [body.evidence_observation_ids, acceptedById],
  );

  return (
    <div className="team-profile">
      {(body.role_observed || body.working_hours_observed) && (
        <div className="team-profile-strip">
          {body.role_observed && (
            <span className="team-profile-role">{body.role_observed}</span>
          )}
          {body.working_hours_observed && (
            <span className="team-profile-hours">
              {body.working_hours_observed.start_local} →{" "}
              {body.working_hours_observed.end_local}
            </span>
          )}
        </div>
      )}

      {collaborators.length > 0 && (
        <section className="team-profile-section">
          <h4 className="home-action-bucket-head">Frequent collaborators</h4>
          <div className="team-profile-chips">
            {collaborators.map((c) => {
              const m = memberById.get(c.person_id);
              if (!m) return null;
              return (
                <button
                  key={c.person_id}
                  type="button"
                  className="team-profile-chip"
                  title={c.evidence}
                  onClick={() => onSelectMember(c.person_id)}
                >
                  <Avatar member={m} size={22} />
                  <span>{m.display_name}</span>
                </button>
              );
            })}
          </div>
        </section>
      )}

      {focus.length > 0 && (
        <section className="team-profile-section">
          <h4 className="home-action-bucket-head">Recent focus</h4>
          <div className="team-profile-chips">
            {focus.map((f) => (
              <button
                key={f.workstream_id}
                type="button"
                className="team-profile-chip"
                onClick={() => onOpenWorkstream(f.workstream_id)}
              >
                <span>{f.title}</span>
              </button>
            ))}
          </div>
        </section>
      )}

      {body.communication_style_notes && (
        <section className="team-profile-section">
          <h4 className="home-action-bucket-head">Communication style</h4>
          <p className="team-profile-style">{body.communication_style_notes}</p>
        </section>
      )}

      {body.last_seen_active_ms !== null && (
        <section className="team-profile-section">
          <h4 className="home-action-bucket-head">Last seen active</h4>
          <p className="team-profile-meta-line">
            {formatRelative(body.last_seen_active_ms)}
          </p>
        </section>
      )}

      {citedObservations.length > 0 && (
        <section className="team-profile-section">
          <h4 className="home-action-bucket-head">Backed by observations</h4>
          <div className="team-profile-chips">
            {citedObservations.map((obs) => (
              <button
                key={obs.id}
                type="button"
                className="team-profile-chip team-profile-citation"
                onClick={() => onCiteClick(obs.id)}
                title={obs.body}
              >
                <span>{truncateText(obs.body, 60)}</span>
              </button>
            ))}
          </div>
        </section>
      )}

      <div className="team-profile-meta">
        <span>Computed {formatRelative(computed_ms)}</span>
        <button
          type="button"
          className="settings-btn"
          onClick={onRefresh}
          disabled={refreshing}
        >
          {refreshing ? "Refreshing…" : "Refresh"}
        </button>
      </div>
    </div>
  );
}

function formatRelative(ms: number): string {
  const diff = Date.now() - ms;
  if (diff < 0) return "just now";
  const secs = Math.floor(diff / 1000);
  if (secs < 60) return "just now";
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;
  const months = Math.floor(days / 30);
  if (months < 12) return `${months}mo ago`;
  const years = Math.floor(months / 12);
  return `${years}y ago`;
}

function truncateText(s: string, cap: number): string {
  if (s.length <= cap) return s;
  return s.slice(0, cap - 1).trimEnd() + "…";
}

// ---------- Suggestions tab (#52) ---------------------------------------

function SuggestionsTab({
  member,
  onOpenNote,
  onChanged,
  highlightId,
  flashKey,
}: {
  member: TeamMember;
  onOpenNote: (path: string) => void;
  onChanged: () => void;
  highlightId: string | null;
  flashKey: number;
}) {
  const [pending, setPending] = useState<ProfileObservation[]>([]);
  const [accepted, setAccepted] = useState<ProfileObservation[]>([]);
  const [rejected, setRejected] = useState<ProfileObservation[]>([]);
  const [citedSet, setCitedSet] = useState<Set<string>>(() => new Set());
  const [showRejected, setShowRejected] = useState(false);
  const [loading, setLoading] = useState(true);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [flashing, setFlashing] = useState<string | null>(null);
  const cardRefs = useRef<Record<string, HTMLElement | null>>({});

  const reload = useCallback(async () => {
    setLoading(true);
    try {
      const [p, a, r, snap] = await Promise.all([
        listProfileObservations(member.id, "pending"),
        listProfileObservations(member.id, "accepted"),
        listProfileObservations(member.id, "rejected"),
        getProfileSnapshot(member.id),
      ]);
      setPending(p);
      setAccepted(a);
      setRejected(r);
      const ids = snap?.body.evidence_observation_ids ?? [];
      setCitedSet(new Set(ids));
    } catch (err) {
      console.error("listProfileObservations failed:", err);
    } finally {
      setLoading(false);
    }
  }, [member.id]);

  useEffect(() => {
    void reload();
  }, [reload]);

  // Scroll-into-view + transient flash when the Profile tab triggers a
  // cross-link. `flashKey` re-fires the effect even when `highlightId`
  // is unchanged (re-click of the same chip).
  useEffect(() => {
    if (loading || !highlightId) return;
    const el = cardRefs.current[highlightId];
    if (!el) return;
    el.scrollIntoView({ behavior: "smooth", block: "center" });
    setFlashing(highlightId);
    const t = window.setTimeout(() => setFlashing(null), 1800);
    return () => window.clearTimeout(t);
  }, [highlightId, flashKey, loading]);

  const runAction = async (id: string, action: () => Promise<void>) => {
    setBusyId(id);
    try {
      await action();
      await reload();
      onChanged();
    } catch (err) {
      console.error("observation action failed:", err);
    } finally {
      setBusyId(null);
    }
  };

  if (loading) return <div className="team-tasks-loading" />;

  // Show the 5 most-recent accepted rows. Then union in any cited rows
  // that fell outside the recent-5 window so the Profile-tab cross-link
  // can always scroll to its target. Cap the merged list at 10.
  const recentAcceptedBase = accepted.slice(0, 5);
  const recentAcceptedIds = new Set(recentAcceptedBase.map((o) => o.id));
  const olderCited = accepted.filter(
    (o) => citedSet.has(o.id) && !recentAcceptedIds.has(o.id),
  );
  const recentAccepted = [...recentAcceptedBase, ...olderCited].slice(0, 10);
  const isEmpty =
    pending.length === 0 && accepted.length === 0 && rejected.length === 0;

  if (isEmpty) {
    return (
      <div className="team-suggestions">
        <p className="home-empty">
          No suggestions yet. Margin proposes observations from new meetings
          with {member.display_name}; they'll appear here for you to accept or
          reject.
        </p>
      </div>
    );
  }

  return (
    <div className="team-suggestions">
      {pending.length > 0 && (
        <section className="team-profile-section">
          <h4 className="home-action-bucket-head">
            Pending ({pending.length})
          </h4>
          <div className="team-suggestion-list">
            {pending.map((obs) => (
              <article key={obs.id} className="team-suggestion-card">
                <p className="team-suggestion-body">{obs.body}</p>
                <div className="team-suggestion-footer">
                  <button
                    type="button"
                    className="team-suggestion-source"
                    onClick={() => onOpenNote(obs.source_note_id)}
                    title={obs.source_note_id}
                  >
                    {obs.source_note_title ?? "Source note"}
                  </button>
                  <div className="team-suggestion-actions">
                    <button
                      type="button"
                      className="team-suggestion-reject"
                      disabled={busyId === obs.id}
                      onClick={() =>
                        void runAction(obs.id, () =>
                          rejectProfileObservation(obs.id),
                        )
                      }
                    >
                      Reject
                    </button>
                    <button
                      type="button"
                      className="team-suggestion-accept"
                      disabled={busyId === obs.id}
                      onClick={() =>
                        void runAction(obs.id, () =>
                          acceptProfileObservation(obs.id),
                        )
                      }
                    >
                      Accept
                    </button>
                  </div>
                </div>
              </article>
            ))}
          </div>
        </section>
      )}

      {recentAccepted.length > 0 && (
        <section className="team-profile-section">
          <h4 className="home-action-bucket-head">Recently accepted</h4>
          <div className="team-suggestion-list">
            {recentAccepted.map((obs) => (
              <article
                key={obs.id}
                ref={(el) => {
                  cardRefs.current[obs.id] = el;
                }}
                className={
                  "team-suggestion-card team-suggestion-accepted" +
                  (flashing === obs.id ? " team-suggestion-flash" : "")
                }
              >
                <p className="team-suggestion-body">{obs.body}</p>
                <div className="team-suggestion-footer">
                  <button
                    type="button"
                    className="team-suggestion-source"
                    onClick={() => onOpenNote(obs.source_note_id)}
                  >
                    {obs.source_note_title ?? "Source note"}
                  </button>
                  <div className="team-suggestion-meta-row">
                    {citedSet.has(obs.id) && (
                      <span
                        className="team-suggestion-cited"
                        title="Cited by current profile"
                      >
                        ✓ Cited
                      </span>
                    )}
                    <span className="team-suggestion-meta">
                      {obs.reviewed_ms !== null
                        ? formatRelative(obs.reviewed_ms)
                        : ""}
                    </span>
                  </div>
                </div>
              </article>
            ))}
          </div>
        </section>
      )}

      {rejected.length > 0 && (
        <section className="team-profile-section">
          <button
            type="button"
            className="team-suggestion-toggle"
            onClick={() => setShowRejected((v) => !v)}
          >
            {showRejected ? "Hide rejected" : `Show rejected (${rejected.length})`}
          </button>
          {showRejected && (
            <div className="team-suggestion-list">
              {rejected.map((obs) => (
                <article
                  key={obs.id}
                  className="team-suggestion-card team-suggestion-accepted"
                >
                  <p className="team-suggestion-body">{obs.body}</p>
                  <div className="team-suggestion-footer">
                    <button
                      type="button"
                      className="team-suggestion-source"
                      onClick={() => onOpenNote(obs.source_note_id)}
                    >
                      {obs.source_note_title ?? "Source note"}
                    </button>
                    <span className="team-suggestion-meta">
                      {obs.reviewed_ms !== null
                        ? formatRelative(obs.reviewed_ms)
                        : ""}
                    </span>
                  </div>
                </article>
              ))}
            </div>
          )}
        </section>
      )}
    </div>
  );
}

// ---------- EditableField -----------------------------------------------

function EditableField({
  value,
  placeholder,
  onCommit,
  className,
  blankFallback,
}: {
  value: string;
  placeholder?: string;
  onCommit: (next: string) => void;
  className?: string;
  /** When `value` is empty AND we're not in edit mode, render this string
   *  in muted form. Useful for required fields (display name) that should
   *  never be visually empty. */
  blankFallback?: string;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(value);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (!editing) setDraft(value);
  }, [value, editing]);

  useEffect(() => {
    if (editing && inputRef.current) {
      inputRef.current.focus();
      inputRef.current.select();
    }
  }, [editing]);

  const commit = () => {
    onCommit(draft);
    setEditing(false);
  };

  if (editing) {
    return (
      <input
        ref={inputRef}
        className={(className ?? "") + " team-field-input"}
        value={draft}
        placeholder={placeholder}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") commit();
          if (e.key === "Escape") {
            setDraft(value);
            setEditing(false);
          }
        }}
      />
    );
  }
  const display = value || blankFallback || "";
  return (
    <button
      type="button"
      className={(className ?? "") + " team-field-display" + (value ? "" : " placeholder")}
      onClick={() => setEditing(true)}
    >
      {display || placeholder || ""}
    </button>
  );
}

// ---------- Tasks tab ----------------------------------------------------

function TasksTab({
  member,
  members,
  onOpenNote,
  onOpenWorkstream,
  onToggleAction,
  onReassignAction,
}: {
  member: TeamMember;
  members: TeamMember[];
  onOpenNote: (path: string) => void;
  onOpenWorkstream: (id: string) => void;
  onToggleAction: (id: string, nextDone: boolean) => void;
  onReassignAction: (actionId: string, memberId: string | null) => Promise<void>;
}) {
  const [actions, setActions] = useState<ActionListItem[] | null>(null);

  useEffect(() => {
    let cancelled = false;
    setActions(null);
    void (async () => {
      try {
        const list = await listActions("all", member.id);
        if (!cancelled) setActions(list);
      } catch (err) {
        console.error("listActions failed:", err);
        if (!cancelled) setActions([]);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [member.id]);

  // Optimistic toggle: flip the local copy immediately so the row
  // updates without a refetch round-trip. The upstream onToggleAction
  // writes to disk; on next reindex / re-render the source of truth
  // matches.
  const toggleLocal = useCallback(
    (id: string, nextDone: boolean) => {
      onToggleAction(id, nextDone);
      setActions((prev) =>
        prev === null
          ? prev
          : prev.map((a) => (a.id === id ? { ...a, done: nextDone } : a)),
      );
    },
    [onToggleAction],
  );

  // Reassign with optimistic chip update + post-write refetch.
  // Reassigning may move the action OUT of this member's tab (if the
  // user picks someone else), so we always refetch after the IPC.
  const reassignLocal = useCallback(
    async (actionId: string, memberId: string | null) => {
      const newName =
        memberId === null
          ? null
          : members.find((m) => m.id === memberId)?.display_name ?? null;
      setActions((prev) =>
        prev === null
          ? prev
          : prev.map((a) =>
              a.id === actionId
                ? {
                    ...a,
                    assignee_id: memberId,
                    assignee_display_name: newName,
                  }
                : a,
            ),
      );
      try {
        await onReassignAction(actionId, memberId);
        // Action ID changes when text changes; refetch to pick it up.
        const fresh = await listActions("all", member.id);
        setActions(fresh);
      } catch (err) {
        console.error("reassign failed:", err);
        try {
          const fresh = await listActions("all", member.id);
          setActions(fresh);
        } catch {}
      }
    },
    [members, member.id, onReassignAction],
  );

  const stats = useMemo(() => {
    if (!actions) return { open: 0, dueThisWeek: 0, overdue: 0 };
    const now = Date.now();
    let open = 0;
    let dueThisWeek = 0;
    let overdue = 0;
    for (const a of actions) {
      if (a.done) continue;
      open += 1;
      if (a.due_ms == null) continue;
      const bucket = dueBucket(a.due_ms, now);
      if (bucket === "overdue") {
        overdue += 1;
        dueThisWeek += 1;
      } else if (bucket === "today" || bucket === "soon") {
        dueThisWeek += 1;
      }
    }
    return { open, dueThisWeek, overdue };
  }, [actions]);

  const grouped = useMemo(() => {
    const empty: Record<string, ActionListItem[]> = {
      overdue: [],
      today: [],
      soon: [],
      later: [],
    };
    const undated: ActionListItem[] = [];
    if (!actions) return { byBucket: empty, undated };
    const now = Date.now();
    for (const a of actions) {
      if (a.done) continue;
      if (a.due_ms == null) {
        undated.push(a);
        continue;
      }
      const bucket = dueBucket(a.due_ms, now);
      empty[bucket].push(a);
    }
    return { byBucket: empty, undated };
  }, [actions]);

  if (actions === null) {
    return <div className="team-tasks-loading" />;
  }

  const totalOpen = stats.open;
  const completed = actions.filter((a) => a.done);

  return (
    <div className="team-tasks-tab">
      <div className="team-tasks-counters">
        <span className="team-tasks-counter">
          <strong>{stats.open}</strong> open
        </span>
        <span className="team-tasks-counter">
          <strong>{stats.dueThisWeek}</strong> due this week
        </span>
        <span
          className={
            "team-tasks-counter overdue" + (stats.overdue > 0 ? " active" : "")
          }
        >
          <strong>{stats.overdue}</strong> overdue
        </span>
      </div>

      {totalOpen === 0 && completed.length === 0 ? (
        <p className="home-empty">
          No tasks attributed to {member.display_name} yet. Action items resolve
          here when they're written as <code>{member.display_name} — task</code>{" "}
          in any meeting note.
        </p>
      ) : (
        <>
          {BUCKET_ORDER.map(({ key, label }) => {
            const items = grouped.byBucket[key];
            if (!items || items.length === 0) return null;
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
                      onToggle={toggleLocal}
                      onOpenNote={onOpenNote}
                      onOpenWorkstream={onOpenWorkstream}
                      members={members}
                      onReassign={(id, memberId) => void reassignLocal(id, memberId)}
                    />
                  ))}
                </div>
              </div>
            );
          })}
          {grouped.undated.length > 0 && (
            <div className="home-action-bucket bucket-undated">
              <div className="home-action-bucket-head">
                <span className="home-action-bucket-label">No due date</span>
                <span className="home-action-bucket-count">
                  {grouped.undated.length}
                </span>
              </div>
              <div className="home-actions">
                {grouped.undated.map((it) => (
                  <ActionRow
                    key={it.id}
                    it={it}
                    onToggle={toggleLocal}
                    onOpenNote={onOpenNote}
                    onOpenWorkstream={onOpenWorkstream}
                    members={members}
                    onReassign={(id, memberId) => void reassignLocal(id, memberId)}
                  />
                ))}
              </div>
            </div>
          )}
          {completed.length > 0 && (
            <div className="home-action-bucket bucket-done">
              <div className="home-action-bucket-head">
                <span className="home-action-bucket-label">Completed</span>
                <span className="home-action-bucket-count">{completed.length}</span>
              </div>
              <div className="home-actions">
                {completed.map((it) => (
                  <ActionRow
                    key={it.id}
                    it={it}
                    onToggle={toggleLocal}
                    onOpenNote={onOpenNote}
                    onOpenWorkstream={onOpenWorkstream}
                    members={members}
                    onReassign={(id, memberId) => void reassignLocal(id, memberId)}
                  />
                ))}
              </div>
            </div>
          )}
        </>
      )}
    </div>
  );
}

// ---------- Identities modal --------------------------------------------

const ALIAS_KIND_LABELS: Array<{ value: string; label: string }> = [
  { value: AliasKind.Email, label: "Email" },
  { value: AliasKind.Name, label: "Name" },
  { value: AliasKind.GithubLogin, label: "GitHub login" },
  { value: AliasKind.SlackId, label: "Slack ID" },
];

function identitiesSummary(aliases: TypedAlias[]): string {
  if (aliases.length === 0) return "Manage identities…";
  if (aliases.length === 1) return `1 identity · manage…`;
  return `${aliases.length} identities · manage…`;
}

type Draft = { kind: string; value: string };

function IdentitiesModal({
  aliases,
  onClose,
  onSave,
}: {
  aliases: TypedAlias[];
  onClose: () => void;
  onSave: (next: TypedAlias[]) => Promise<void> | void;
}) {
  const [drafts, setDrafts] = useState<Draft[]>(() =>
    aliases.map((a) => ({ kind: a.kind, value: a.value })),
  );
  const [saving, setSaving] = useState(false);

  // Esc closes; backdrop click closes. Save / Add affordances are
  // explicit buttons inside the modal.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const updateRow = (i: number, patch: Partial<Draft>) => {
    setDrafts((prev) =>
      prev.map((d, idx) => (idx === i ? { ...d, ...patch } : d)),
    );
  };

  const removeRow = (i: number) => {
    setDrafts((prev) => prev.filter((_, idx) => idx !== i));
  };

  const addRow = () => {
    setDrafts((prev) => [...prev, { kind: AliasKind.Email, value: "" }]);
  };

  const handleSave = async () => {
    // Filter empty values, dedupe (kind, value) pairs client-side. The
    // backend's PRIMARY KEY also enforces this, but trimming here keeps
    // the optimistic state aligned with what the server will store.
    const cleaned: TypedAlias[] = [];
    const seen = new Set<string>();
    for (const d of drafts) {
      const kind = d.kind.trim();
      const value = d.value.trim();
      if (!kind || !value) continue;
      const key = `${kind}\x00${value}`;
      if (seen.has(key)) continue;
      seen.add(key);
      cleaned.push({ kind, value });
    }
    setSaving(true);
    try {
      await onSave(cleaned);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div
      className="identities-modal-backdrop"
      role="presentation"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="identities-modal"
        role="dialog"
        aria-modal="true"
        aria-label="Manage identities"
      >
        <header className="identities-modal-header">
          <h2>Manage identities</h2>
          <p className="identities-modal-help">
            Each identity is tagged with a kind so connectors (email, GitHub,
            Slack…) can resolve people back to this team member.
          </p>
        </header>
        <div className="identities-modal-rows">
          {drafts.length === 0 && (
            <div className="identities-modal-empty">
              No identities yet — add an email or connector handle below.
            </div>
          )}
          {drafts.map((d, i) => (
            <div className="identities-modal-row" key={i}>
              <select
                className="identities-modal-kind"
                value={d.kind}
                onChange={(e) => updateRow(i, { kind: e.target.value })}
              >
                {ALIAS_KIND_LABELS.map((opt) => (
                  <option key={opt.value} value={opt.value}>
                    {opt.label}
                  </option>
                ))}
              </select>
              <input
                className="identities-modal-value"
                type="text"
                placeholder={placeholderFor(d.kind)}
                value={d.value}
                onChange={(e) => updateRow(i, { value: e.target.value })}
                autoFocus={i === drafts.length - 1 && d.value === ""}
              />
              <button
                type="button"
                className="identities-modal-remove"
                onClick={() => removeRow(i)}
                aria-label="Remove identity"
              >
                <IconTrash size={13} sw={1.8} />
              </button>
            </div>
          ))}
        </div>
        <div className="identities-modal-add-row">
          <button
            type="button"
            className="identities-modal-add"
            onClick={addRow}
          >
            <IconPlus size={13} sw={1.8} />
            Add identity
          </button>
        </div>
        <footer className="identities-modal-footer">
          <button
            type="button"
            className="identities-modal-cancel"
            onClick={onClose}
            disabled={saving}
          >
            Cancel
          </button>
          <button
            type="button"
            className="identities-modal-save"
            onClick={() => void handleSave()}
            disabled={saving}
          >
            {saving ? "Saving…" : "Save"}
          </button>
        </footer>
      </div>
    </div>
  );
}

function placeholderFor(kind: string): string {
  switch (kind) {
    case AliasKind.Email:
      return "name@example.com";
    case AliasKind.Name:
      return "First Last (or nickname)";
    case AliasKind.GithubLogin:
      return "github-handle";
    case AliasKind.SlackId:
      return "U0ABCDE12";
    default:
      return "value";
  }
}
