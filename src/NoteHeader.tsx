import { useEffect, useRef, useState } from "react";
import {
  IconArchive,
  IconCalendar,
  IconChevLeft,
  IconCopy,
  IconEdit,
  IconEye,
  IconFolder,
  IconHome,
  IconLink,
  IconMore,
  IconPlus,
  IconSettings,
  IconShare,
  IconSparkle,
  IconStar,
  IconTrash,
} from "./icons";

export type EditorSettings = {
  /** "tabs" | "spaces" — derived from useTabs in App.tsx */
  indent: "Spaces" | "Tabs";
  /** Tab width — "2" | "4" | "8" */
  width: "2" | "4" | "8";
  /** "Soft wrap" | "No wrap" */
  wrap: "Soft wrap" | "No wrap";
};

type Props = {
  /** Derived from the markdown body's first H1; falls back to filename. */
  title: string;
  /** Called when the user edits the title. Caller rewrites the H1 line. */
  onTitleChange: (next: string) => void;
  /** "edit" | "preview" */
  mode: "edit" | "preview";
  onModeChange: (next: "edit" | "preview") => void;
  /** Whether a recording is currently active (mic capture running). */
  recording: boolean;
  /** True iff Record can be started right now (owned note + idle state). */
  canRecord: boolean;
  onStartRecord: () => void;
  onStopRecord: () => void;
  /** Editor settings (indent/width/wrap). */
  settings: EditorSettings;
  onSettingsChange: (next: EditorSettings) => void;
  /** Created/modified timestamp for the date chip. */
  modifiedMs: number | null;
  onBack: () => void;
};

export function NoteHeader({
  title,
  onTitleChange,
  mode,
  onModeChange,
  recording,
  canRecord,
  onStartRecord,
  onStopRecord,
  settings,
  onSettingsChange,
  modifiedMs,
  onBack,
}: Props) {
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [moreOpen, setMoreOpen] = useState(false);

  // Close popovers on any outside click.
  useEffect(() => {
    if (!settingsOpen && !moreOpen) return;
    const close = () => {
      setSettingsOpen(false);
      setMoreOpen(false);
    };
    window.addEventListener("mousedown", close);
    return () => window.removeEventListener("mousedown", close);
  }, [settingsOpen, moreOpen]);

  return (
    <header className="note-header">
      <div className="note-header-row1" data-tauri-drag-region>
        <Breadcrumb noteTitle={title} onBack={onBack} />
        <div className="note-header-spacer" />
        <ViewModeToggle mode={mode} onChange={onModeChange} />
        <div className="note-header-divider" />
        <RecordButton
          recording={recording}
          disabled={!canRecord && !recording}
          onClick={recording ? onStopRecord : onStartRecord}
        />

        <div className="nh-popover-anchor">
          <button
            type="button"
            className={"nh-icon-btn" + (settingsOpen ? " active" : "")}
            aria-label="Editor settings"
            title="Editor settings"
            onClick={(e) => {
              e.stopPropagation();
              setSettingsOpen((v) => !v);
              setMoreOpen(false);
            }}
          >
            <IconSettings size={15} sw={1.7} />
          </button>
          {settingsOpen && (
            <SettingsPopover settings={settings} onChange={onSettingsChange} />
          )}
        </div>

        <ShareCluster
          onShare={() => stub("Share", 15)}
          onCopyLink={() => stub("Copy link", 15)}
        />

        <div className="nh-popover-anchor">
          <button
            type="button"
            className={"nh-icon-btn" + (moreOpen ? " active" : "")}
            aria-label="More"
            title="More"
            onClick={(e) => {
              e.stopPropagation();
              setMoreOpen((v) => !v);
              setSettingsOpen(false);
            }}
          >
            <IconMore size={16} sw={1.8} />
          </button>
          {moreOpen && <MoreMenu onClose={() => setMoreOpen(false)} />}
        </div>
      </div>

      <div className="note-header-row2">
        <EditableTitle value={title} onChange={onTitleChange} />
        <div className="nh-chips">
          <button
            type="button"
            className="nh-chip"
            title="Folders coming soon — see issue #13"
            onClick={() => stub("Folder", 13)}
          >
            <IconFolder size={12} sw={1.7} />
            <span>Add to folder</span>
          </button>
          {modifiedMs !== null && (
            <span className="nh-chip" title="Modified">
              <IconCalendar size={12} sw={1.7} />
              <span>{formatModifiedAt(modifiedMs)}</span>
            </span>
          )}
          <button
            type="button"
            className="nh-chip-add"
            title="Tags coming soon — see issue #14"
            aria-label="Add tag"
            onClick={() => stub("Tag", 14)}
          >
            <IconPlus size={12} sw={1.8} />
          </button>
        </div>
      </div>
    </header>
  );
}

function Breadcrumb({ noteTitle, onBack }: { noteTitle: string; onBack: () => void }) {
  return (
    <div className="nh-breadcrumb">
      <button
        type="button"
        className="nh-breadcrumb-back"
        title="Back to all notes"
        onClick={onBack}
      >
        <IconChevLeft size={14} sw={1.8} />
        <IconHome size={14} sw={1.6} />
      </button>
      <span className="nh-breadcrumb-sep">/</span>
      <span className="nh-breadcrumb-title">{noteTitle}</span>
    </div>
  );
}

function ViewModeToggle({
  mode,
  onChange,
}: {
  mode: "edit" | "preview";
  onChange: (m: "edit" | "preview") => void;
}) {
  const opts: { id: "edit" | "preview"; label: string; icon: React.ReactNode }[] = [
    { id: "edit", label: "Edit", icon: <IconEdit size={13} sw={1.7} /> },
    { id: "preview", label: "Preview", icon: <IconEye size={13} sw={1.7} /> },
  ];
  return (
    <div className="nh-segmented" role="tablist" aria-label="View mode">
      {opts.map((o) => (
        <button
          key={o.id}
          type="button"
          role="tab"
          aria-selected={mode === o.id}
          className={"nh-segmented-btn" + (mode === o.id ? " active" : "")}
          onClick={() => onChange(o.id)}
        >
          {o.icon}
          {o.label}
        </button>
      ))}
    </div>
  );
}

function RecordButton({
  recording,
  disabled,
  onClick,
}: {
  recording: boolean;
  disabled: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      className={"nh-record" + (recording ? " recording" : "")}
      onClick={onClick}
      disabled={disabled}
    >
      <span className="nh-record-dot" />
      {recording ? "Recording…" : "Record"}
    </button>
  );
}

function SettingsPopover({
  settings,
  onChange,
}: {
  settings: EditorSettings;
  onChange: (next: EditorSettings) => void;
}) {
  const rows: {
    key: keyof EditorSettings;
    label: string;
    opts: readonly string[];
  }[] = [
    { key: "indent", label: "Indent", opts: ["Spaces", "Tabs"] },
    { key: "width", label: "Width", opts: ["2", "4", "8"] },
    { key: "wrap", label: "Wrap", opts: ["Soft wrap", "No wrap"] },
  ];
  return (
    <div
      className="nh-popover nh-settings-popover"
      onMouseDown={(e) => e.stopPropagation()}
    >
      {rows.map((row) => (
        <div key={row.key} className="nh-settings-row">
          <span className="nh-settings-row-label">{row.label}</span>
          <div className="nh-settings-seg">
            {row.opts.map((o) => {
              const on = settings[row.key] === o;
              return (
                <button
                  key={o}
                  type="button"
                  className={"nh-settings-seg-btn" + (on ? " active" : "")}
                  onClick={() =>
                    onChange({ ...settings, [row.key]: o } as EditorSettings)
                  }
                >
                  {o}
                </button>
              );
            })}
          </div>
        </div>
      ))}
    </div>
  );
}

function ShareCluster({
  onShare,
  onCopyLink,
}: {
  onShare: () => void;
  onCopyLink: () => void;
}) {
  return (
    <div className="nh-share">
      <button
        type="button"
        className="nh-share-main"
        onClick={onShare}
        title="Share — coming soon (issue #15)"
      >
        <IconShare size={13} sw={1.8} />
        Share
      </button>
      <div className="nh-share-divider" />
      <button
        type="button"
        className="nh-share-link"
        onClick={onCopyLink}
        aria-label="Copy link"
        title="Copy link — coming soon (issue #15)"
      >
        <IconLink size={13} sw={1.7} />
      </button>
    </div>
  );
}

function MoreMenu({ onClose }: { onClose: () => void }) {
  type Item =
    | { id: "sep" }
    | {
        id: string;
        label: string;
        icon: React.ReactNode;
        issue: number;
        danger?: boolean;
      };
  const items: Item[] = [
    { id: "fav", icon: <IconStar size={14} />, label: "Add to favorites", issue: 16 },
    { id: "dup", icon: <IconCopy size={14} />, label: "Duplicate", issue: 18 },
    { id: "ai", icon: <IconSparkle size={14} />, label: "Summarize with AI", issue: 19 },
    { id: "sep" },
    { id: "arc", icon: <IconArchive size={14} />, label: "Archive", issue: 17 },
    { id: "del", icon: <IconTrash size={14} />, label: "Delete", issue: 20, danger: true },
  ];
  return (
    <div
      className="nh-popover nh-more-popover"
      onMouseDown={(e) => e.stopPropagation()}
    >
      {items.map((it) =>
        "label" in it ? (
          <button
            key={it.id}
            type="button"
            className={"nh-more-item" + (it.danger ? " danger" : "")}
            title={`${it.label} — coming soon (issue #${it.issue})`}
            onClick={() => {
              stub(it.label, it.issue);
              onClose();
            }}
          >
            {it.icon}
            <span>{it.label}</span>
          </button>
        ) : (
          <div key="sep" className="nh-more-sep" />
        ),
      )}
    </div>
  );
}

function EditableTitle({
  value,
  onChange,
}: {
  value: string;
  onChange: (next: string) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(value);
  const inputRef = useRef<HTMLInputElement>(null);

  // Keep the local draft in sync when the source title changes (e.g. user
  // edits the H1 directly in the markdown body).
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
    const v = draft.trim() || "Untitled";
    if (v !== value) onChange(v);
    setDraft(v);
    setEditing(false);
  };

  if (editing) {
    return (
      <input
        ref={inputRef}
        className="nh-title"
        value={draft}
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
  return (
    <h1 className="nh-title" onClick={() => setEditing(true)}>
      {value}
    </h1>
  );
}

// --- helpers ---

function stub(label: string, issue: number) {
  console.log(`[stub] ${label} clicked — see issue #${issue}`);
}

function formatModifiedAt(ms: number): string {
  const d = new Date(ms);
  const now = new Date();
  const sameDay = d.toDateString() === now.toDateString();
  const yesterday = new Date(now.getTime() - 86_400_000);
  const wasYesterday = d.toDateString() === yesterday.toDateString();
  const time = d.toLocaleTimeString(undefined, {
    hour: "numeric",
    minute: "2-digit",
  });
  if (sameDay) return `Today · ${time}`;
  if (wasYesterday) return `Yesterday · ${time}`;
  const sameYear = d.getFullYear() === now.getFullYear();
  const date = d.toLocaleDateString(undefined, {
    weekday: "short",
    month: "short",
    day: "numeric",
    year: sameYear ? undefined : "numeric",
  });
  return `${date} · ${time}`;
}
