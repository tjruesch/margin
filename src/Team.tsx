import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ask } from "@tauri-apps/plugin-dialog";

import { IconChevLeft, IconPlus, IconSearch, IconTrash } from "./icons";
import { usePageDetailLifecycle } from "./pageDetail";
import {
  AliasKind,
  type ProfileObservation,
  type ProfileSnapshot,
  type ProfileSnapshotBody,
  type TeamMember,
  type TypedAlias,
  acceptProfileObservation,
  countProfileSnapshots,
  createTeamMember,
  deleteTeamMember,
  forceRecomputeProfile,
  getFirstProfileSnapshot,
  getProfileSnapshot,
  getProfileSnapshotAt,
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
}: {
  onOpenNote: (path: string) => void;
  /** Routes workstream-sourced rows to the Workstreams view (#100). */
  onOpenWorkstream: (id: string) => void;
}) {
  const [members, setMembers] = useState<TeamMember[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [pendingCounts, setPendingCounts] = useState<Record<string, number>>({});
  // Cross-link state (#115, lifted up for #116). Owned here so the
  // activity-popover → team-detail jump can seed both the highlight
  // and the active tab without per-mount races.
  const [highlightObsId, setHighlightObsId] = useState<string | null>(null);
  const [flashKey, setFlashKey] = useState(0);
  const [pendingTab, setPendingTab] = useState<
    "profile" | "suggestions" | null
  >(null);

  const onCiteClick = useCallback((obsId: string) => {
    setHighlightObsId(obsId);
    setFlashKey((k) => k + 1);
    setPendingTab("suggestions");
  }, []);

  // Cross-app navigation from ActivityPanel (#116). The event detail
  // carries a memberId (required) and an optional highlightObsId. We
  // set selectedId so the TeamDetail mounts, and seed the highlight
  // + tab so the Suggestions row scrolls into view + flashes.
  useEffect(() => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent).detail as
        | { memberId?: string; highlightObsId?: string | null }
        | undefined;
      if (!detail?.memberId) return;
      setSelectedId(detail.memberId);
      if (detail.highlightObsId) {
        setHighlightObsId(detail.highlightObsId);
        setFlashKey((k) => k + 1);
        setPendingTab("suggestions");
      } else {
        setPendingTab("profile");
      }
    };
    window.addEventListener("margin:open-team-member", handler);
    return () =>
      window.removeEventListener("margin:open-team-member", handler);
  }, []);

  const reload = useCallback(async () => {
    const fresh = await listTeamMembers();
    setMembers(fresh);
  }, []);

  const reloadCounts = useCallback(async () => {
    try {
      const pending = await pendingObservationCounts();
      setPendingCounts(pending);
    } catch (err) {
      console.error("team counts fetch failed:", err);
    }
  }, []);

  useEffect(() => {
    void reloadCounts();
  }, [reloadCounts]);

  // Tell App.tsx (and anyone else holding a member-list copy) that the
  // roster changed. Dispatched after any create / update / delete so
  // member-name lookups across the app see the new list without forcing
  // a full app reload.
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
        highlightObsId={highlightObsId}
        flashKey={flashKey}
        pendingTab={pendingTab}
        onPendingTabConsumed={() => setPendingTab(null)}
        onBack={() => setSelectedId(null)}
        onSelectMember={setSelectedId}
        onCiteClick={onCiteClick}
        onOpenNote={onOpenNote}
        onOpenWorkstream={onOpenWorkstream}
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
  const [query, setQuery] = useState("");

  // The "Add team member" trigger lives in PageHeader (Home.tsx) when
  // nav === "team"; we listen for its dispatched event and open the
  // inline composer below.
  useEffect(() => {
    const onOpen = () => setComposerOpen(true);
    window.addEventListener("margin:open-team-composer", onOpen);
    return () =>
      window.removeEventListener("margin:open-team-composer", onOpen);
  }, []);

  const q = query.trim().toLowerCase();
  const filtered = useMemo(() => {
    if (!q) return members;
    return members.filter((m) => {
      if (m.display_name.toLowerCase().includes(q)) return true;
      if (m.role.toLowerCase().includes(q)) return true;
      if (m.aliases.some((a) => a.value.toLowerCase().includes(q))) return true;
      return false;
    });
  }, [members, q]);

  const self = !q ? filtered.find((m) => m.is_self) : undefined;
  const team = !q ? filtered.filter((m) => !m.is_self) : filtered;

  return (
    <section className="home-section team-list-pane">
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
          their name can be recognized across your notes and meetings.
        </p>
      ) : (
        <>
          <div className="team-list-toolbar">
            <label className="team-list-search">
              <IconSearch size={13} sw={1.8} />
              <input
                type="search"
                placeholder="Search team…"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                aria-label="Search team"
              />
            </label>
            <span className="team-list-count">
              {q
                ? `${filtered.length} of ${members.length}`
                : `${members.length} ${
                    members.length === 1 ? "member" : "members"
                  }`}
            </span>
          </div>

          <div className="team-list-table-head" role="row" aria-hidden>
            <span className="team-list-col team-list-col-person">Person</span>
            <span
              className="team-list-col team-list-col-indicator"
              title="Waiting on you"
            >
              On you
            </span>
            <span
              className="team-list-col team-list-col-indicator"
              title="Waiting on them"
            >
              On them
            </span>
            <span
              className="team-list-col team-list-col-indicator"
              title="Pending suggestions"
            >
              Suggest.
            </span>
          </div>

          <div className="team-list-scroll">
            {filtered.length === 0 ? (
              <p className="home-empty">
                No matches for &ldquo;{query}&rdquo;.
              </p>
            ) : (
              <>
                {self && (
                  <div className="team-list-group">
                    <h4 className="team-list-group-head">You</h4>
                    <div className="team-list">
                      <TeamRow
                        key={self.id}
                        member={self}
                        pendingCount={pendingCounts[self.id] ?? 0}
                        onClick={() => onSelect(self.id)}
                      />
                    </div>
                  </div>
                )}
                {team.length > 0 && (
                  <div className="team-list-group">
                    {!q && (
                      <h4 className="team-list-group-head">
                        Team
                        <span className="team-list-group-count">
                          {team.length}
                        </span>
                      </h4>
                    )}
                    <div className="team-list">
                      {team.map((m) => (
                        <TeamRow
                          key={m.id}
                          member={m}
                          pendingCount={pendingCounts[m.id] ?? 0}
                          onClick={() => onSelect(m.id)}
                        />
                      ))}
                    </div>
                  </div>
                )}
              </>
            )}
          </div>
        </>
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
      <div className="team-row-person">
        <Avatar member={member} size={36} />
        <div className="team-row-body">
          <div className="team-row-name">
            {member.display_name}
            {member.is_self && <span className="team-self-badge">You</span>}
          </div>
          <div className="team-row-role">
            {member.role || (member.is_self ? "Your profile" : "—")}
          </div>
        </div>
      </div>
      <RowIndicator
        count={pendingCount}
        tone="suggestion"
        label={`${pendingCount} pending suggestion${pendingCount === 1 ? "" : "s"}`}
      />
    </div>
  );
}

function RowIndicator({
  count,
  tone,
  label,
}: {
  count: number;
  tone: "on-you" | "on-them" | "suggestion";
  label: string;
}) {
  if (count <= 0) {
    return <span className="team-row-indicator team-row-indicator-empty">—</span>;
  }
  return (
    <span
      className={`team-row-indicator team-row-indicator-${tone}`}
      title={label}
      aria-label={label}
    >
      {count}
    </span>
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
  highlightObsId,
  flashKey,
  pendingTab,
  onPendingTabConsumed,
  onBack,
  onSelectMember,
  onCiteClick,
  onUpdated,
  onDeleted,
  onOpenNote,
  onOpenWorkstream,
  onObservationsChanged,
}: {
  member: TeamMember;
  members: TeamMember[];
  pendingCount: number;
  highlightObsId: string | null;
  flashKey: number;
  pendingTab: "profile" | "suggestions" | null;
  onPendingTabConsumed: () => void;
  onBack: () => void;
  onSelectMember: (id: string) => void;
  onCiteClick: (obsId: string) => void;
  onUpdated: (next: TeamMember) => void;
  onDeleted: () => void;
  onOpenNote: (path: string) => void;
  onOpenWorkstream: (id: string) => void;
  onObservationsChanged: () => void;
}) {
  const [tab, setTab] = useState<"profile" | "suggestions">("profile");
  // Tell Home.tsx to drop its page-level H1 + list actions for as long
  // as this detail view is mounted (#117-ish navigation polish).
  usePageDetailLifecycle();

  // Reset to Profile when switching to a different member so the user
  // never lands on a stale tab from the previous selection.
  useEffect(() => {
    setTab("profile");
  }, [member.id]);

  // Consume a one-shot pending tab seed from the parent (set by either
  // a citation chip click or a cross-app navigation from ActivityPanel).
  useEffect(() => {
    if (pendingTab) {
      setTab(pendingTab);
      onPendingTabConsumed();
    }
  }, [pendingTab, onPendingTabConsumed]);

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
      `Delete ${member.display_name}? Their profile and any meeting attendance records are removed.`,
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
      <div className="detail-topbar">
        <button
          type="button"
          className="detail-crumb"
          onClick={onBack}
        >
          <IconChevLeft size={13} sw={1.8} />
          Team
        </button>
        {!member.is_self && (
          <button
            type="button"
            className="detail-action-icon detail-action-danger"
            onClick={() => void onDelete()}
            aria-label="Delete team member"
            title="Delete"
          >
            <IconTrash size={14} sw={1.8} />
          </button>
        )}
      </div>
      <header className="team-detail-header">
        <Avatar member={member} size={48} />
        <div className="team-detail-fields">
          <EditableField
            value={member.display_name}
            placeholder="Name"
            onCommit={(v) => void commitName(v)}
            className="team-detail-name"
            blankFallback={member.display_name}
          />
          <div className="team-detail-sub">
            <EditableField
              value={member.role}
              placeholder="Add a role"
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
      <div className="team-detail-tabs home-filter" role="tablist" aria-label="Section">
        <button
          type="button"
          role="tab"
          aria-selected={tab === "profile"}
          className={"home-filter-chip" + (tab === "profile" ? " active" : "")}
          onClick={() => setTab("profile")}
        >
          Profile
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={tab === "suggestions"}
          className={
            "home-filter-chip" + (tab === "suggestions" ? " active" : "")
          }
          onClick={() => setTab("suggestions")}
        >
          Suggestions
          {pendingCount > 0 && (
            <span className="actions-filter-count">{pendingCount}</span>
          )}
        </button>
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

// ---------- Profile snapshot diff helpers (#118) -------------------------

type CompareTo = "none" | "7d" | "30d" | "first";

/// Pure-LCS word-level diff (#133). Each segment carries the unbroken
/// run of tokens that share an op so consecutive add/del words render
/// inside one span (cleaner DOM than per-word spans).
///
/// Returns `null` when:
///   - either side is too long (>500 chars) — pathological renders
///   - the diff has more than 30 add/del segments — looks like a full
///     rewrite; the stacked render is more legible
/// Callers fall back to the existing stacked prev/current render in
/// that case. See #133's acceptance criteria.
type WordDiffOp = "keep" | "add" | "del";
type WordDiffSegment = { op: WordDiffOp; text: string };

const WORD_DIFF_MAX_LEN = 500;
const WORD_DIFF_MAX_CHANGES = 30;

function tokenizeForDiff(s: string): string[] {
  // Whitespace and non-whitespace runs as separate tokens. Whitespace
  // tokens act as anchors in the LCS (they almost always match) so
  // typo-fixes inside a paragraph collapse to a single add+del pair.
  return s.match(/\s+|\S+/g) ?? [];
}

function wordDiff(prev: string, curr: string): WordDiffSegment[] | null {
  if (prev.length > WORD_DIFF_MAX_LEN || curr.length > WORD_DIFF_MAX_LEN) {
    return null;
  }
  const a = tokenizeForDiff(prev);
  const b = tokenizeForDiff(curr);
  const n = a.length;
  const m = b.length;
  // Standard LCS DP table; O(n*m) time + space. Bounded by the length
  // gate above (≤500 chars ≈ ≤200 tokens each side → 40k cells).
  const dp: number[][] = Array.from({ length: n + 1 }, () =>
    new Array<number>(m + 1).fill(0),
  );
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] = a[i] === b[j]
        ? dp[i + 1][j + 1] + 1
        : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const segs: WordDiffSegment[] = [];
  const push = (op: WordDiffOp, text: string) => {
    const last = segs[segs.length - 1];
    if (last && last.op === op) last.text += text;
    else segs.push({ op, text });
  };
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      push("keep", a[i]);
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      push("del", a[i]);
      i++;
    } else {
      push("add", b[j]);
      j++;
    }
  }
  while (i < n) push("del", a[i++]);
  while (j < m) push("add", b[j++]);

  // Count change segments (add+del). One typo-fix = 2 segments. >30
  // change segments is closer to a rewrite than an edit — the stacked
  // render is more legible there.
  const changes = segs.reduce(
    (n, s) => n + (s.op === "keep" ? 0 : 1),
    0,
  );
  if (changes > WORD_DIFF_MAX_CHANGES) return null;
  return segs;
}

/// Inline word-level diff renderer (#133). Falls back to the stacked
/// prev/current pair when the diff is too large to render usefully
/// (see `wordDiff` cap rules). The fallback path matches the legacy
/// render so callers can swap this in without losing the long-text
/// behaviour established in #118.
function ProseDiff({
  prev,
  curr,
  className,
}: {
  prev: string;
  curr: string;
  className: string;
}) {
  const segs = useMemo(() => wordDiff(prev, curr), [prev, curr]);
  if (segs == null) {
    // Stacked fallback — preserves the #118 legibility contract for
    // long prev/curr pairs and near-full rewrites.
    return (
      <>
        <p className={`${className} team-profile-diff-prev`}>{prev}</p>
        <p className={className}>{curr}</p>
      </>
    );
  }
  return (
    <p className={className}>
      {segs.map((s, i) => {
        if (s.op === "keep") return <span key={i}>{s.text}</span>;
        if (s.op === "del") {
          return (
            <span key={i} className="team-profile-diff-del">
              {s.text}
            </span>
          );
        }
        return (
          <span key={i} className="team-profile-diff-add">
            {s.text}
          </span>
        );
      })}
    </p>
  );
}

/// Set-diff a pair of keyed arrays into added / removed / unchanged
/// + rank-shifted items (#118). Rank shifts trigger only when an
/// item exists in both arrays at materially different positions
/// (delta >= 2) — anything tighter is noise from small reorderings.
function diffByKey<T>(
  prev: readonly T[],
  next: readonly T[],
  key: (t: T) => string,
): {
  added: T[];
  removed: T[];
  unchanged: T[];
  rankByKey: Map<string, { prevRank: number; nextRank: number }>;
} {
  const prevByKey = new Map(prev.map((p, i) => [key(p), { item: p, rank: i }]));
  const nextByKey = new Map(next.map((n, i) => [key(n), { item: n, rank: i }]));
  const added: T[] = [];
  const removed: T[] = [];
  const unchanged: T[] = [];
  const rankByKey = new Map<string, { prevRank: number; nextRank: number }>();
  for (const n of next) {
    const k = key(n);
    if (prevByKey.has(k)) {
      unchanged.push(n);
      const prevRank = prevByKey.get(k)!.rank;
      const nextRank = nextByKey.get(k)!.rank;
      if (Math.abs(prevRank - nextRank) >= 2) {
        rankByKey.set(k, { prevRank, nextRank });
      }
    } else {
      added.push(n);
    }
  }
  for (const p of prev) {
    if (!nextByKey.has(key(p))) removed.push(p);
  }
  return { added, removed, unchanged, rankByKey };
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

  const { body, computed_ms, person_id } = snap;
  const subject = memberById.get(person_id);

  // Compare-to state (#118). When `compareTo !== 'none'`, fetch the
  // older snapshot and diff per field. Empty-state when only one
  // snapshot exists for this member.
  const [compareTo, setCompareTo] = useState<CompareTo>("none");
  const [compareSnap, setCompareSnap] = useState<ProfileSnapshot | null>(null);
  const [snapshotCount, setSnapshotCount] = useState<number>(1);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const n = await countProfileSnapshots(person_id);
        if (!cancelled) setSnapshotCount(n);
      } catch (e) {
        console.error("countProfileSnapshots failed:", e);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [person_id]);

  // Reset compareTo when navigating to a different member.
  useEffect(() => {
    setCompareTo("none");
    setCompareSnap(null);
  }, [person_id]);

  useEffect(() => {
    let cancelled = false;
    if (compareTo === "none") {
      setCompareSnap(null);
      return;
    }
    void (async () => {
      try {
        let result: ProfileSnapshot | null = null;
        if (compareTo === "first") {
          result = await getFirstProfileSnapshot(person_id);
          // Only useful if the first snapshot is actually older than
          // the current one.
          if (result && result.computed_ms >= computed_ms) result = null;
        } else {
          const days = compareTo === "7d" ? 7 : 30;
          const cutoff = Date.now() - days * 24 * 3600 * 1000;
          result = await getProfileSnapshotAt(person_id, cutoff);
        }
        if (!cancelled) setCompareSnap(result);
      } catch (e) {
        console.error("getProfileSnapshotAt failed:", e);
        if (!cancelled) setCompareSnap(null);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [compareTo, person_id, computed_ms]);

  const compareBody: ProfileSnapshotBody | null = compareSnap?.body ?? null;

  // The team_members.role is already shown in the page header. Suppress
  // the snapshot's observed-role echo when it agrees; only surface it
  // when the AI has noticed a drift.
  const observedRoleDiffers =
    body.role_observed != null &&
    body.role_observed.trim().toLowerCase() !==
      (subject?.role ?? "").trim().toLowerCase();
  // Frequent collaborators sourced from the user's own data graph will
  // include the user themselves. Drop the self row — listing the reader
  // as their colleague's "frequent collaborator" makes no sense. The
  // deeper reframe of this field is a future issue.
  const collaborators = (body.frequent_collaborators ?? []).filter((c) => {
    const m = memberById.get(c.person_id);
    return m != null && !m.is_self;
  });
  const focus = body.recent_focus ?? [];

  // Per-field diffs (#118). When compareBody is null, every diff is
  // empty — the existing render is the unchanged path through the
  // same JSX.
  const focusDiff = useMemo(
    () =>
      diffByKey(
        compareBody?.recent_focus ?? [],
        focus,
        (f) => f.workstream_id,
      ),
    [compareBody, focus],
  );
  const collabDiff = useMemo(
    () =>
      diffByKey(
        (compareBody?.frequent_collaborators ?? []).filter((c) => {
          const m = memberById.get(c.person_id);
          return m != null && !m.is_self;
        }),
        collaborators,
        (c) => c.person_id,
      ),
    [compareBody, collaborators, memberById],
  );
  const roleChanged =
    compareBody != null &&
    (compareBody.role_observed ?? "") !== (body.role_observed ?? "");
  const styleChanged =
    compareBody != null &&
    (compareBody.communication_style_notes ?? "") !==
      (body.communication_style_notes ?? "");
  const summaryChanged =
    compareBody != null &&
    (compareBody.summary_prose ?? "") !== (body.summary_prose ?? "");
  const workingHoursChanged =
    compareBody != null &&
    JSON.stringify(compareBody.working_hours_observed ?? null) !==
      JSON.stringify(body.working_hours_observed ?? null);
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
  const firstName = (subject?.display_name ?? "").split(" ")[0] || "them";

  const hasStrip =
    observedRoleDiffers ||
    body.working_hours_observed != null ||
    body.last_seen_active_ms != null;

  return (
    <div className="team-profile">
      {/* Zone 1 — Hero. The summary is the page's centerpiece (large,
          high-contrast prose). Meta strip floats above as small
          muted attributes — never compete with the summary. */}
      <header className="team-profile-hero">
        {hasStrip && (
          <div className="team-profile-strip">
            {observedRoleDiffers && body.role_observed && (
              <span className="team-profile-strip-emphasis">
                {roleChanged && compareBody?.role_observed && (
                  <span className="team-profile-diff-prev">
                    {compareBody.role_observed}
                  </span>
                )}
                <span>{body.role_observed}</span>
              </span>
            )}
            {body.working_hours_observed && (
              <span className="team-profile-strip-item">
                {workingHoursChanged && compareBody?.working_hours_observed && (
                  <span className="team-profile-diff-prev">
                    {compareBody.working_hours_observed.start_local} →{" "}
                    {compareBody.working_hours_observed.end_local}
                  </span>
                )}
                {body.working_hours_observed.start_local} →{" "}
                {body.working_hours_observed.end_local}
              </span>
            )}
            {body.last_seen_active_ms != null && (
              <span className="team-profile-strip-item">
                Last active {formatRelative(body.last_seen_active_ms)}
              </span>
            )}
          </div>
        )}
        {body.summary_prose ? (
          <div className="team-profile-summary-block">
            {summaryChanged && compareBody?.summary_prose ? (
              <ProseDiff
                prev={compareBody.summary_prose}
                curr={body.summary_prose}
                className="team-profile-summary"
              />
            ) : (
              <p className="team-profile-summary">{body.summary_prose}</p>
            )}
          </div>
        ) : (
          <p className="team-profile-summary team-profile-summary--placeholder">
            A short portrait of {firstName} appears here once Margin has
            enough signal — focus, working style, and how to work well
            together.
          </p>
        )}
      </header>

      {/* Zone 3 — At-a-glance. Quieter visual weight; supports the
          hero without competing with it. Sections only render when
          their data is non-empty. */}
      <div className="team-profile-glance">
        {(focus.length > 0 || focusDiff.removed.length > 0) && (
          <section className="team-profile-glance-row">
            <h4 className="team-profile-glance-label">Working on</h4>
            <div className="team-profile-chips">
              {focus.map((f) => {
                const isAdded =
                  compareBody != null &&
                  focusDiff.added.some(
                    (x) => x.workstream_id === f.workstream_id,
                  );
                const shift = focusDiff.rankByKey.get(f.workstream_id);
                return (
                  <button
                    key={f.workstream_id}
                    type="button"
                    className={
                      "team-profile-chip" +
                      (isAdded ? " team-profile-chip--added" : "")
                    }
                    onClick={() => onOpenWorkstream(f.workstream_id)}
                  >
                    <span>{f.title}</span>
                    {shift && (
                      <span className="team-profile-diff-rank">
                        {shift.nextRank < shift.prevRank ? "↑" : "↓"}
                      </span>
                    )}
                  </button>
                );
              })}
              {focusDiff.removed.map((f) => (
                <button
                  key={`-${f.workstream_id}`}
                  type="button"
                  className="team-profile-chip team-profile-chip--removed"
                  onClick={() => onOpenWorkstream(f.workstream_id)}
                  title="Dropped since the comparison snapshot"
                >
                  <span>{f.title}</span>
                </button>
              ))}
            </div>
          </section>
        )}

        {(collaborators.length > 0 || collabDiff.removed.length > 0) && (
          <section className="team-profile-glance-row">
            <h4 className="team-profile-glance-label">Often with</h4>
            <div className="team-profile-chips">
              {collaborators.map((c) => {
                const m = memberById.get(c.person_id);
                if (!m) return null;
                const isAdded =
                  compareBody != null &&
                  collabDiff.added.some((x) => x.person_id === c.person_id);
                const shift = collabDiff.rankByKey.get(c.person_id);
                return (
                  <button
                    key={c.person_id}
                    type="button"
                    className={
                      "team-profile-chip team-profile-chip--avatar" +
                      (isAdded ? " team-profile-chip--added" : "")
                    }
                    title={c.evidence}
                    onClick={() => onSelectMember(c.person_id)}
                  >
                    <Avatar member={m} size={18} />
                    <span>{m.display_name}</span>
                    {shift && (
                      <span className="team-profile-diff-rank">
                        {shift.nextRank < shift.prevRank ? "↑" : "↓"}
                      </span>
                    )}
                  </button>
                );
              })}
              {collabDiff.removed.map((c) => {
                const m = memberById.get(c.person_id);
                if (!m) return null;
                return (
                  <button
                    key={`-${c.person_id}`}
                    type="button"
                    className="team-profile-chip team-profile-chip--avatar team-profile-chip--removed"
                    onClick={() => onSelectMember(c.person_id)}
                    title="No longer cited as a frequent collaborator"
                  >
                    <Avatar member={m} size={18} />
                    <span>{m.display_name}</span>
                  </button>
                );
              })}
            </div>
          </section>
        )}

        {body.communication_style_notes && (
          <section className="team-profile-glance-row">
            <h4 className="team-profile-glance-label">Communication style</h4>
            {styleChanged && compareBody?.communication_style_notes ? (
              <ProseDiff
                prev={compareBody.communication_style_notes}
                curr={body.communication_style_notes}
                className="team-profile-style"
              />
            ) : (
              <p className="team-profile-style">
                {body.communication_style_notes}
              </p>
            )}
          </section>
        )}

        {citedObservations.length > 0 && (
          <section className="team-profile-glance-row">
            <h4 className="team-profile-glance-label">Backed by</h4>
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
      </div>

      <div className="team-profile-meta">
        <span>Computed {formatRelative(computed_ms)}</span>
        {snapshotCount > 1 ? (
          <label className="team-profile-compare">
            Compare to:
            <select
              className="settings-btn team-profile-compare-select"
              value={compareTo}
              onChange={(e) => setCompareTo(e.target.value as CompareTo)}
            >
              <option value="none">Latest only</option>
              <option value="7d">7 days ago</option>
              <option value="30d">30 days ago</option>
              <option value="first">First snapshot</option>
            </select>
          </label>
        ) : (
          <span className="team-profile-compare-empty">
            Only one snapshot — give the worker time to record changes.
          </span>
        )}
        {compareTo !== "none" && compareSnap == null && (
          <span className="team-profile-compare-empty">
            No earlier snapshot available.
          </span>
        )}
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

// ---------- Identities modal --------------------------------------------

const ALIAS_KIND_LABELS: Array<{ value: string; label: string }> = [
  { value: AliasKind.Email, label: "Email" },
  { value: AliasKind.Name, label: "Name" },
  { value: AliasKind.GithubLogin, label: "GitHub login" },
  { value: AliasKind.SlackId, label: "Slack ID" },
];

function identitiesSummary(aliases: TypedAlias[]): string {
  if (aliases.length === 0) return "Add identity";
  if (aliases.length === 1) return "1 identity";
  return `${aliases.length} identities`;
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
