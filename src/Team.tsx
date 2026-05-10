import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ask } from "@tauri-apps/plugin-dialog";

import { dueBucket } from "./dueLabel";
import { Editor } from "./Editor";
import { ActionRow, BUCKET_ORDER } from "./Home";
import { IconChevLeft, IconPlus, IconTrash } from "./icons";
import {
  AliasKind,
  type ActionListItem,
  type TeamMember,
  type TypedAlias,
  createTeamMember,
  deleteTeamMember,
  listActions,
  listTeamMembers,
  readFile,
  updateTeamMember,
  writeFile,
} from "./file";
import { avatarColor, initialsFromName } from "./initials";

export type EditorSettings = {
  tabSize: number;
  useTabs: boolean;
  softWrap: boolean;
  fontSize: number;
};

// Stable empty array for Editor's `actions` prop — profile.md bodies
// don't carry action items, but the prop is required by Editor's type.
// Reusing the same reference avoids busting the Editor's `useMemo` for
// extensions on every TeamView re-render.
const EMPTY_ACTIONS: ActionListItem[] = [];

export function TeamView({
  editor,
  onOpenNote,
  onToggleAction,
  onReassignAction,
}: {
  editor: EditorSettings;
  onOpenNote: (path: string) => void;
  onToggleAction: (id: string, nextDone: boolean) => void;
  onReassignAction: (actionId: string, memberId: string | null) => Promise<void>;
}) {
  const [members, setMembers] = useState<TeamMember[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const reload = useCallback(async () => {
    const fresh = await listTeamMembers();
    setMembers(fresh);
  }, []);

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
        editor={editor}
        onBack={() => setSelectedId(null)}
        onOpenNote={onOpenNote}
        onToggleAction={onToggleAction}
        onReassignAction={onReassignAction}
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
  onSelect,
  onCreated,
}: {
  members: TeamMember[];
  onSelect: (id: string) => void;
  onCreated: (member: TeamMember) => void;
}) {
  const [composerOpen, setComposerOpen] = useState(false);
  return (
    <section className="home-section">
      <div className="home-section-head">
        <div>
          <div className="home-section-eyebrow">Team</div>
          <h2 className="home-section-title">Your team</h2>
        </div>
        {!composerOpen && (
          <button
            type="button"
            className="home-section-add"
            onClick={() => setComposerOpen(true)}
          >
            <IconPlus size={12} sw={1.8} />
            Add team member
          </button>
        )}
      </div>
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
            <TeamRow key={m.id} member={m} onClick={() => onSelect(m.id)} />
          ))}
        </div>
      )}
    </section>
  );
}

function TeamRow({ member, onClick }: { member: TeamMember; onClick: () => void }) {
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

const PROFILE_SAVE_DEBOUNCE_MS = 600;

function TeamDetail({
  member,
  members,
  editor,
  onBack,
  onUpdated,
  onDeleted,
  onOpenNote,
  onToggleAction,
  onReassignAction,
}: {
  member: TeamMember;
  members: TeamMember[];
  editor: EditorSettings;
  onBack: () => void;
  onUpdated: (next: TeamMember) => void;
  onDeleted: () => void;
  onOpenNote: (path: string) => void;
  onToggleAction: (id: string, nextDone: boolean) => void;
  onReassignAction: (actionId: string, memberId: string | null) => Promise<void>;
}) {
  const [body, setBody] = useState<string | null>(null);
  const pendingBody = useRef<string | null>(null);
  const saveTimer = useRef<number | null>(null);
  const profilePath = member.profile_md_path;
  const [tab, setTab] = useState<"profile" | "tasks">("profile");

  // Reset to Profile when switching to a different member so the user
  // never lands on a stale Tasks tab from the previous selection.
  useEffect(() => {
    setTab("profile");
  }, [member.id]);

  // Load profile.md whenever the targeted member changes.
  useEffect(() => {
    let cancelled = false;
    setBody(null);
    pendingBody.current = null;
    void (async () => {
      try {
        const file = await readFile(profilePath);
        if (!cancelled) setBody(file.content);
      } catch (err) {
        console.error("read profile failed:", err);
        if (!cancelled) setBody("");
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [profilePath]);

  // Debounced flush. On unmount or member-switch, write any pending body
  // immediately so we don't lose the user's last keystrokes.
  const flushPending = useCallback(() => {
    if (saveTimer.current !== null) {
      window.clearTimeout(saveTimer.current);
      saveTimer.current = null;
    }
    const next = pendingBody.current;
    if (next === null) return;
    pendingBody.current = null;
    void writeFile(profilePath, next).catch((err) => {
      console.error("write profile failed:", err);
    });
  }, [profilePath]);

  useEffect(() => {
    return () => {
      flushPending();
    };
  }, [flushPending]);

  const onBodyChange = (next: string) => {
    setBody(next);
    pendingBody.current = next;
    if (saveTimer.current !== null) window.clearTimeout(saveTimer.current);
    saveTimer.current = window.setTimeout(() => {
      saveTimer.current = null;
      const value = pendingBody.current;
      if (value === null) return;
      pendingBody.current = null;
      void writeFile(profilePath, value).catch((err) => {
        console.error("write profile failed:", err);
      });
    }, PROFILE_SAVE_DEBOUNCE_MS);
  };

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
    flushPending();
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
          onClick={() => {
            flushPending();
            onBack();
          }}
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
            aria-selected={tab === "tasks"}
            className={"nh-segmented-btn" + (tab === "tasks" ? " active" : "")}
            onClick={() => setTab("tasks")}
          >
            Tasks
          </button>
        </div>
      </div>
      {/* Profile body — hidden, not unmounted, so the editor's debounced
          save and the EditableField drafts survive a Tasks-tab detour. */}
      <div
        className="team-detail-editor"
        style={{ display: tab === "profile" ? undefined : "none" }}
      >
        {body !== null && (
          <Editor
            value={body}
            onChange={onBodyChange}
            tabSize={editor.tabSize}
            useTabs={editor.useTabs}
            softWrap={editor.softWrap}
            fontSize={editor.fontSize}
            actions={EMPTY_ACTIONS}
          />
        )}
      </div>
      {tab === "tasks" && (
        <TasksTab
          member={member}
          members={members}
          onOpenNote={onOpenNote}
          onToggleAction={onToggleAction}
          onReassignAction={onReassignAction}
        />
      )}
    </section>
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
  onToggleAction,
  onReassignAction,
}: {
  member: TeamMember;
  members: TeamMember[];
  onOpenNote: (path: string) => void;
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
