import { useEffect, useRef, useState } from "react";
import {
  IconCalendar,
  IconChevLeft,
  IconEdit,
  IconEye,
  IconFileText,
  IconHome,
  IconLink,
  IconMore,
  IconPlus,
  IconShare,
} from "./icons";
import { MoreMenu } from "./MoreMenu";

export type ViewMode = "edit" | "preview" | "transcript";

type Props = {
  /** Derived from the markdown body's first H1; falls back to filename. */
  title: string;
  /** Called when the user edits the title. Caller rewrites the H1 line. */
  onTitleChange: (next: string) => void;
  mode: ViewMode;
  onModeChange: (next: ViewMode) => void;
  /** Whether a transcript exists for the active note — gates the third
   *  segment in the view-mode toggle. */
  hasTranscript: boolean;
  /** Whether a recording is currently active (mic capture running). */
  recording: boolean;
  /** True iff Record can be started right now (owned note + idle state). */
  canRecord: boolean;
  onStartRecord: () => void;
  onStopRecord: () => void;
  /** Created/modified timestamp for the date chip. */
  modifiedMs: number | null;
  /** Tags for the active note. Empty for external (non-owned) notes. */
  tags: string[];
  /** Autocomplete pool — every tag known across all owned notes. */
  allTags: string[];
  /** External notes don't carry tags; the chip cluster degrades to none. */
  tagsEditable: boolean;
  onTagsChange: (next: string[]) => void;
  onBack: () => void;
  /** When omitted, the Delete item is hidden from the More menu. The
   *  parent decides eligibility (owned bundle + idle recording state). */
  onDelete?: () => void;
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
  hasTranscript,
  modifiedMs,
  tags,
  allTags,
  tagsEditable,
  onTagsChange,
  onBack,
  onDelete,
}: Props) {
  const [moreOpen, setMoreOpen] = useState(false);

  useEffect(() => {
    if (!moreOpen) return;
    const close = () => setMoreOpen(false);
    window.addEventListener("mousedown", close);
    return () => window.removeEventListener("mousedown", close);
  }, [moreOpen]);

  return (
    <header className="note-header">
      <div className="note-header-row1" data-tauri-drag-region>
        <button
          type="button"
          className="nh-back"
          title="Back to all notes"
          aria-label="Back to all notes"
          onClick={onBack}
        >
          <IconChevLeft size={14} sw={1.8} />
          <IconHome size={14} sw={1.6} />
        </button>
        <EditableTitle value={title} onChange={onTitleChange} />
        <RecordButton
          recording={recording}
          disabled={!canRecord && !recording}
          onClick={recording ? onStopRecord : onStartRecord}
        />

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
            }}
          >
            <IconMore size={16} sw={1.8} />
          </button>
          {moreOpen && (
            <MoreMenu onClose={() => setMoreOpen(false)} onDelete={onDelete} />
          )}
        </div>
      </div>

      <div className="note-header-row2">
        <div className="nh-chips">
          {modifiedMs !== null && (
            <span className="nh-chip" title="Modified">
              <IconCalendar size={12} sw={1.7} />
              <span>{formatModifiedAt(modifiedMs)}</span>
            </span>
          )}
          {tagsEditable && (
            <TagCluster
              tags={tags}
              allTags={allTags}
              onChange={onTagsChange}
            />
          )}
        </div>
        <ViewModeToggle
          mode={mode}
          onChange={onModeChange}
          hasTranscript={hasTranscript}
        />
      </div>
    </header>
  );
}

function ViewModeToggle({
  mode,
  onChange,
  hasTranscript,
}: {
  mode: ViewMode;
  onChange: (m: ViewMode) => void;
  hasTranscript: boolean;
}) {
  const opts: { id: ViewMode; label: string; icon: React.ReactNode }[] = [
    { id: "edit", label: "Edit", icon: <IconEdit size={13} sw={1.7} /> },
    { id: "preview", label: "Preview", icon: <IconEye size={13} sw={1.7} /> },
  ];
  if (hasTranscript) {
    opts.push({
      id: "transcript",
      label: "Transcript",
      icon: <IconFileText size={13} sw={1.7} />,
    });
  }
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

function TagCluster({
  tags,
  allTags,
  onChange,
}: {
  tags: string[];
  allTags: string[];
  onChange: (next: string[]) => void;
}) {
  const [open, setOpen] = useState(false);
  const [draft, setDraft] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);
  const popoverRef = useRef<HTMLDivElement>(null);

  // Close on outside click. Don't trip when the click lands inside the
  // popover (lets the user pick suggestions without dismissing).
  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      const target = e.target as Node | null;
      if (popoverRef.current && target && popoverRef.current.contains(target)) return;
      setOpen(false);
    };
    window.addEventListener("mousedown", close);
    return () => window.removeEventListener("mousedown", close);
  }, [open]);

  useEffect(() => {
    if (open) inputRef.current?.focus();
  }, [open]);

  const removeTag = (t: string) => {
    onChange(tags.filter((x) => x !== t));
  };
  const addTag = (t: string) => {
    const norm = normalizeTag(t);
    if (!norm) return;
    if (tags.includes(norm)) return;
    onChange([...tags, norm]);
    setDraft("");
  };

  const draftNorm = normalizeTag(draft);
  const suggestions = draftNorm
    ? allTags.filter((t) => t.startsWith(draftNorm) && !tags.includes(t)).slice(0, 6)
    : allTags.filter((t) => !tags.includes(t)).slice(0, 6);

  return (
    <>
      {tags.map((t) => (
        <button
          key={t}
          type="button"
          className="nh-chip nh-chip-tag"
          title={`Remove tag ${t}`}
          onClick={() => removeTag(t)}
        >
          <span>{t}</span>
          <span className="nh-chip-tag-x" aria-hidden="true">×</span>
        </button>
      ))}
      <div className="nh-popover-anchor">
        <button
          type="button"
          className={"nh-chip-add" + (open ? " active" : "")}
          aria-label="Add tag"
          title="Add tag"
          onClick={(e) => {
            e.stopPropagation();
            setOpen((v) => !v);
          }}
        >
          <IconPlus size={12} sw={1.8} />
        </button>
        {open && (
          <div
            ref={popoverRef}
            className="nh-popover nh-tag-popover"
            onMouseDown={(e) => e.stopPropagation()}
          >
            <input
              ref={inputRef}
              className="nh-tag-input"
              placeholder="Add a tag…"
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  addTag(suggestions[0] ?? draft);
                } else if (e.key === "Escape") {
                  setOpen(false);
                }
              }}
            />
            {suggestions.length > 0 && (
              <div className="nh-tag-suggestions">
                {suggestions.map((s) => (
                  <button
                    key={s}
                    type="button"
                    className="nh-tag-suggestion"
                    onClick={() => addTag(s)}
                  >
                    {s}
                  </button>
                ))}
              </div>
            )}
          </div>
        )}
      </div>
    </>
  );
}

const TAG_MAX_LEN = 32;

function normalizeTag(raw: string): string {
  return raw.trim().toLowerCase().slice(0, TAG_MAX_LEN);
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
