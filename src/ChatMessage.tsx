//! Shared message-rendering components for the cmd+K palette and the
//! full-page Chat surface. Extracted from SearchPalette so both
//! surfaces render the same chrome (chips, tool pills, cited links)
//! against the same provider state. CSS class names match the existing
//! `palette-*` styles — both surfaces reuse them.
//!
//! Display model: an assistant message is an ordered `MessagePart[]`
//! where text and tool-use markers interleave in arrival order, plus
//! a flat `sources` array used for chip rendering and `[N]` citation
//! resolution. User messages collapse to a single text part.

import { lazy, Suspense, useMemo, useState } from "react";
import type React from "react";

import {
  type AskSource,
  type MessagePart,
  openOrCreateEventNote,
} from "./file";
import { render as renderMarkdown } from "./markdown";
import { IconCheck } from "./icons";

// Lazy-import so the inspector's React tree doesn't ship with the
// initial chat bundle — users open it on demand.
const PromptInspector = lazy(() =>
  import("./PromptInspector").then((m) => ({ default: m.PromptInspector })),
);

/** Display shape — same as the SearchPalette's local `ChatMessage`. */
export type ChatMessageView = {
  id: string;
  role: "user" | "assistant";
  parts: MessagePart[];
  sources?: AskSource[];
  status: "streaming" | "done" | "error";
  error?: string;
  /** Correlates with the Tauri `ai-stream` channel's `turn_id`. */
  turnId?: string;
};

/// Concatenate all text parts of a message. Used for citation parsing
/// (regex over the whole prose) and for shipping history back to the
/// model (which only stores plain text content).
export function joinText(parts: MessagePart[]): string {
  let out = "";
  for (const p of parts) {
    if (p.kind === "text") out += p.value;
  }
  return out;
}

/// CSS variant class keyed off source kind. Centralized so inline `[N]`
/// markers and the bottom "Sources" strip stay consistent.
export function chipVariant(kind: AskSource["kind"]): string {
  switch (kind) {
    case "event":
      return "is-event";
    case "workstream":
      return "is-workstream";
    case "teams_message":
      return "is-teams-message";
    case "email":
      return "is-email";
    case "note":
    default:
      return "is-note";
  }
}

/// Open the surface a source points at. Notes go through `onOpenNote`
/// directly. Events route through `openOrCreateEventNote` (#62) which
/// creates the linked bundle on first click. Workstreams hand the id to
/// `onOpenWorkstream`, which switches the sidebar nav and dispatches
/// `margin:open-workstream` so the detail view selects this one (#72).
export async function openSource(
  source: AskSource,
  onOpenNote: (path: string) => void,
  onOpenWorkstream: (workstreamId: string) => void,
): Promise<void> {
  if (source.kind === "note" && source.note_path) {
    onOpenNote(source.note_path);
    return;
  }
  if (source.kind === "event" && source.event_id) {
    try {
      const path = await openOrCreateEventNote(source.event_id);
      onOpenNote(path);
    } catch (e) {
      console.error("[ask] open event note failed:", e);
    }
    return;
  }
  if (source.kind === "workstream" && source.workstream_id) {
    onOpenWorkstream(source.workstream_id);
    return;
  }
  if (source.kind === "teams_message") {
    // v1: route to the message's attached workstream when available;
    // unattached messages are a soft no-op until we have a dedicated
    // Teams-message viewer (#136 follow-up).
    if (source.workstream_id) {
      onOpenWorkstream(source.workstream_id);
    }
    return;
  }
  if (source.kind === "email") {
    // Same v1 strategy as Teams messages (#137): route to the attached
    // workstream if any; unattached emails are a soft no-op until we
    // have a dedicated email viewer. Chip still renders + shows the
    // sender + subject on hover.
    if (source.workstream_id) {
      onOpenWorkstream(source.workstream_id);
    }
  }
}

export function Conversation({
  ref,
  messages,
  onOpenNote,
  onOpenWorkstream,
}: {
  ref?: React.RefObject<HTMLDivElement | null>;
  messages: ChatMessageView[];
  onOpenNote: (path: string) => void;
  onOpenWorkstream: (workstreamId: string) => void;
}) {
  return (
    <div className="palette-conversation" ref={ref}>
      {messages.map((m) => (
        <MessageBubble
          key={m.id}
          message={m}
          onOpenNote={onOpenNote}
          onOpenWorkstream={onOpenWorkstream}
        />
      ))}
    </div>
  );
}

export function MessageBubble({
  message,
  onOpenNote,
  onOpenWorkstream,
}: {
  message: ChatMessageView;
  onOpenNote: (path: string) => void;
  onOpenWorkstream: (workstreamId: string) => void;
}) {
  if (message.role === "user") {
    return (
      <div className="palette-msg palette-msg-user">
        <div className="palette-msg-bubble">{joinText(message.parts)}</div>
      </div>
    );
  }
  if (message.status === "error") {
    return (
      <div className="palette-msg palette-msg-assistant">
        <div className="palette-msg-bubble palette-msg-error">
          {message.error || "Something went wrong."}
        </div>
      </div>
    );
  }
  const sources = message.sources || [];
  const textParts = useMemo(
    () =>
      message.parts.filter(
        (p): p is Extract<MessagePart, { kind: "text" }> => p.kind === "text",
      ),
    [message.parts],
  );
  const toolParts = useMemo(
    () =>
      message.parts.filter(
        (p): p is Extract<MessagePart, { kind: "tool" }> => p.kind === "tool",
      ),
    [message.parts],
  );
  const fullText = useMemo(() => joinText(message.parts), [message.parts]);
  // Render chips only for labels the model actually cited across all
  // text parts. The full source surface can be hundreds of entries;
  // showing all of them would be a wall of chips. Matches `[N]`,
  // `[E<N>]`, `[W<N>]`, `[T<N>]`, and `[U<N>]`.
  const citedSources = useMemo(() => {
    if (sources.length === 0) return [];
    const cited = new Set<string>();
    const re = /\[([WETU]?\d{1,3})\]/g;
    let m: RegExpExecArray | null;
    while ((m = re.exec(fullText)) !== null) {
      cited.add(m[1]);
    }
    return sources.filter((s) => cited.has(s.label));
  }, [sources, fullText]);

  const isStreaming = message.status === "streaming";
  const isEmptyAndStreaming = message.parts.length === 0 && isStreaming;
  // Drives the single-line "Reading X" indicator below the bubble. The
  // latest tool part wins — replaces the previous as the model moves on.
  const latestTool = toolParts.length > 0 ? toolParts[toolParts.length - 1] : null;
  const [inspectorOpen, setInspectorOpen] = useState(false);
  // 🔍 button shows only on done assistant turns that have a turn_id
  // (pre-#134 history might lack one). Hidden while streaming because
  // the dump isn't persisted until the terminal `done` event.
  const canInspect =
    !isStreaming && message.status === "done" && !!message.turnId;

  return (
    <div className="palette-msg palette-msg-assistant">
      <div className="palette-msg-bubble">
        {isEmptyAndStreaming && textParts.length === 0 ? (
          <span className="palette-msg-typing">
            <span></span>
            <span></span>
            <span></span>
          </span>
        ) : (
          textParts.map((part, i) => (
            <CitedText
              key={i}
              text={part.value}
              sources={sources}
              onOpenNote={onOpenNote}
              onOpenWorkstream={onOpenWorkstream}
            />
          ))
        )}
        {canInspect && (
          <button
            type="button"
            className="palette-msg-inspect"
            title="Inspect what the AI saw for this turn"
            aria-label="Open prompt inspector"
            onClick={() => setInspectorOpen(true)}
          >
            🔍
          </button>
        )}
      </div>
      {isStreaming && latestTool ? (
        <ToolStatus
          part={latestTool}
          onOpen={() => {
            const src = sources.find((s) => s.label === latestTool.targetLabel);
            if (!src) return;
            void openSource(src, onOpenNote, onOpenWorkstream);
          }}
        />
      ) : !isStreaming && citedSources.length > 0 ? (
        <SourcesStrip
          sources={citedSources}
          onOpen={(s) => void openSource(s, onOpenNote, onOpenWorkstream)}
        />
      ) : null}
      {inspectorOpen && (
        <Suspense fallback={null}>
          <PromptInspector
            message={message}
            onClose={() => setInspectorOpen(false)}
          />
        </Suspense>
      )}
    </div>
  );
}

/// Single-line "Reading [X] 'Title'…" indicator that renders below the
/// bubble while a tool call is in flight. Replaces itself as new tool
/// calls fire — only the most recent is shown. After streaming finishes
/// this disappears and `SourcesStrip` takes over.
function ToolStatus({
  part,
  onOpen,
}: {
  part: Extract<MessagePart, { kind: "tool" }>;
  onOpen: () => void;
}) {
  const verb =
    part.name === "read_event_details"
      ? "Reading event"
      : part.name === "read_workstream"
        ? "Reading workstream"
        : part.name === "read_transcript"
          ? "Reading transcript"
          : part.name === "read_teams_message"
            ? "Reading message"
            : part.name === "read_email"
              ? "Reading email"
              : "Reading";
  const label = part.targetLabel || `${part.targetN}`;
  const titleAttr = `${part.name}([${label}] ${part.targetTitle})`;
  return (
    <button
      type="button"
      className={`palette-tool-status status-${part.status}`}
      title={titleAttr}
      onClick={onOpen}
    >
      <span className="palette-tool-icon" aria-hidden="true">
        {part.status === "ok" ? (
          <IconCheck size={11} sw={2} />
        ) : part.status === "error" ? (
          "✗"
        ) : (
          <span className="palette-tool-spinner" />
        )}
      </span>
      <span className="palette-tool-text">
        {verb} <span className="palette-tool-target">[{label}]</span>
        {part.targetTitle ? ` "${part.targetTitle}"` : ""}
      </span>
    </button>
  );
}

/// Collapsed-by-default "Sources (N)" affordance with a chevron. Click
/// expands to the chip strip — same chip shape as before, but tucked
/// behind a single header line so a long answer with many citations
/// doesn't dominate the message column.
function SourcesStrip({
  sources,
  onOpen,
}: {
  sources: AskSource[];
  onOpen: (source: AskSource) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  return (
    <div className="palette-sources">
      <button
        type="button"
        className="palette-sources-toggle"
        aria-expanded={expanded}
        onClick={() => setExpanded((v) => !v)}
      >
        <span className="palette-sources-chev">{expanded ? "▾" : "▸"}</span>
        Sources ({sources.length})
      </button>
      {expanded && (
        <div className="palette-sources-list">
          {sources.map((s) => (
            <button
              key={s.label}
              type="button"
              className={`palette-source-chip ${chipVariant(s.kind)}`}
              title={s.title}
              onClick={() => onOpen(s)}
            >
              <span className="palette-source-num">{s.label}</span>
              <span className="palette-source-title">{s.title}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

/// Render assistant text as Markdown with `[N]` markers swapped for
/// clickable chips that link to the corresponding source. Replaces each
/// `[N]` with a sentinel `<button>` BEFORE markdown-it sees the source,
/// so the inline HTML passes through untouched. Click handling is
/// delegated on the wrapper div so one handler covers every chip.
function CitedText({
  text,
  sources,
  onOpenNote,
  onOpenWorkstream,
}: {
  text: string;
  sources: AskSource[];
  onOpenNote: (path: string) => void;
  onOpenWorkstream: (workstreamId: string) => void;
}) {
  const sourceByLabel = useMemo(() => {
    const m = new Map<string, AskSource>();
    for (const s of sources) m.set(s.label, s);
    return m;
  }, [sources]);

  const html = useMemo(() => {
    const withCitations = text.replace(/\[([WETU]?\d{1,3})\]/g, (full, label) => {
      const src = sourceByLabel.get(label);
      if (!src) return full;
      return `<button type="button" class="palette-cite ${chipVariant(src.kind)}" data-cite-label="${label}">${label}</button>`;
    });
    return renderMarkdown(withCitations);
  }, [text, sourceByLabel]);

  const onClick = (e: React.MouseEvent<HTMLDivElement>) => {
    const target = e.target as HTMLElement;
    const cite = target.closest<HTMLElement>(".palette-cite[data-cite-label]");
    if (!cite) return;
    const label = cite.getAttribute("data-cite-label");
    if (!label) return;
    const source = sourceByLabel.get(label);
    if (source) void openSource(source, onOpenNote, onOpenWorkstream);
  };

  return (
    <div
      className="palette-md"
      onClick={onClick}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
}
