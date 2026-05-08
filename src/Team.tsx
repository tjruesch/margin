import { useCallback, useEffect, useRef, useState } from "react";
import { ask } from "@tauri-apps/plugin-dialog";

import { Editor } from "./Editor";
import { IconChevLeft, IconPlus, IconTrash } from "./icons";
import {
  type TeamMember,
  createTeamMember,
  deleteTeamMember,
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

export function TeamView({ editor }: { editor: EditorSettings }) {
  const [members, setMembers] = useState<TeamMember[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const reload = useCallback(async () => {
    const fresh = await listTeamMembers();
    setMembers(fresh);
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
        editor={editor}
        onBack={() => setSelectedId(null)}
        onUpdated={(next) =>
          setMembers((prev) => prev.map((m) => (m.id === next.id ? next : m)))
        }
        onDeleted={() => {
          setMembers((prev) => prev.filter((m) => m.id !== member.id));
          setSelectedId(null);
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
  return (
    <section className="home-section">
      <div className="home-section-head">
        <div>
          <div className="home-section-eyebrow">Team</div>
          <h2 className="home-section-title">Your team</h2>
        </div>
      </div>
      <TeamComposer onCreated={onCreated} />
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

function TeamComposer({ onCreated }: { onCreated: (member: TeamMember) => void }) {
  const [open, setOpen] = useState(false);
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const inputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    if (open) inputRef.current?.focus();
  }, [open]);

  const submit = async () => {
    const trimmed = text.trim();
    if (!trimmed || busy) return;
    setBusy(true);
    try {
      const member = await createTeamMember(trimmed, "", []);
      setText("");
      setOpen(false);
      onCreated(member);
    } catch (err) {
      console.error("createTeamMember failed:", err);
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
        Add team member
      </button>
    );
  }

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
            setOpen(false);
            setText("");
          }
        }}
      />
      <div className="team-composer-actions">
        <button
          type="button"
          className="inbox-composer-cancel"
          onClick={() => {
            setOpen(false);
            setText("");
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
  editor,
  onBack,
  onUpdated,
  onDeleted,
}: {
  member: TeamMember;
  editor: EditorSettings;
  onBack: () => void;
  onUpdated: (next: TeamMember) => void;
  onDeleted: () => void;
}) {
  const [body, setBody] = useState<string | null>(null);
  const pendingBody = useRef<string | null>(null);
  const saveTimer = useRef<number | null>(null);
  const profilePath = member.profile_md_path;

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

  const commitAliases = async (next: string) => {
    const parsed = next
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);
    if (
      parsed.length === member.aliases.length &&
      parsed.every((v, i) => v === member.aliases[i])
    ) {
      return;
    }
    try {
      const updated = await updateTeamMember(member.id, { aliases: parsed });
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
          <EditableField
            value={member.aliases.join(", ")}
            placeholder="Aliases, comma-separated (e.g. SR, Sara)"
            onCommit={(v) => void commitAliases(v)}
            className="team-detail-aliases"
          />
        </div>
      </header>
      <div className="team-detail-editor">
        {body !== null && (
          <Editor
            value={body}
            onChange={onBodyChange}
            tabSize={editor.tabSize}
            useTabs={editor.useTabs}
            softWrap={editor.softWrap}
            fontSize={editor.fontSize}
          />
        )}
      </div>
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
