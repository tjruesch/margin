import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useEffect, useMemo, useRef, useState } from "react";

import {
  type AiStreamEvent,
  type AskSource,
  askNotesStart,
  type MessagePart,
  openOrCreateEventNote,
  searchNotes,
  SEARCH_HIGHLIGHT_CLOSE,
  SEARCH_HIGHLIGHT_OPEN,
  type SearchHit,
  startVoiceRecording,
  stopVoiceRecording,
} from "./file";
import { LevelMeter } from "./LevelMeter";
import { render as renderMarkdown } from "./markdown";
import {
  IconArrowRight,
  IconCheck,
  IconFileText,
  IconMic,
  IconSearch,
  IconSparkle,
} from "./icons";

type Props = {
  open: boolean;
  onClose: () => void;
  onOpenNote: (path: string) => void;
};

const QUERY_DEBOUNCE_MS = 120;

type ChatMessage = {
  id: string;
  role: "user" | "assistant";
  /** Ordered list of parts. User messages always have a single text
   *  part (the user can't trigger tools directly). Assistant messages
   *  interleave text deltas and tool-use markers in the order the
   *  model emitted them. */
  parts: MessagePart[];
  /** Assistant-only — populated by the `sources` event before any
   *  `delta` arrives so chips can render alongside the streaming text. */
  sources?: AskSource[];
  status: "streaming" | "done" | "error";
  error?: string;
  /** The turn_id returned by `ask_notes_start`; used to filter the
   *  Tauri `ai-stream` channel to events for this message. Only set on
   *  assistant messages. */
  turnId?: string;
};

/// Concatenate all text parts of a message, ignoring tool parts. Used
/// for citation parsing (regex over the whole prose) and for shipping
/// history back to the backend (which only stores plain text content).
function joinText(parts: MessagePart[]): string {
  let out = "";
  for (const p of parts) {
    if (p.kind === "text") out += p.value;
  }
  return out;
}

/// Search palette + AI Q&A surface (#31).
///
/// Two modes share the same dialog:
///   - "search": debounced lexical search via `search_notes`. Plain Enter
///     opens the active row.
///   - "chat":  conversation thread streamed from `ask_notes_start`.
///     Plain Enter submits the next turn.
///
/// Cmd+Enter from search escalates to chat (sends the current input as
/// the first user turn). Esc closes and resets everything.
type VoiceState =
  | { kind: "off" }
  | { kind: "recording" }
  | { kind: "transcribing" }
  | { kind: "didnt-catch"; message?: string };

export function SearchPalette({ open, onClose, onOpenNote }: Props) {
  const [mode, setMode] = useState<"search" | "chat">("search");
  const [query, setQuery] = useState("");
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [loading, setLoading] = useState(false);
  const [activeIdx, setActiveIdx] = useState(0);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [voiceState, setVoiceState] = useState<VoiceState>({ kind: "off" });
  const voiceModeActive = voiceState.kind !== "off";

  const inputRef = useRef<HTMLInputElement | null>(null);
  const dialogRef = useRef<HTMLDivElement | null>(null);
  const resultsRef = useRef<HTMLDivElement | null>(null);
  const conversationRef = useRef<HTMLDivElement | null>(null);
  // Generation counter for the search debounce — out-of-order responses
  // from a slower keystroke must not overwrite a newer one's results.
  const queryGen = useRef(0);

  // Reset everything on close. Focus the input on open.
  useEffect(() => {
    if (!open) {
      setMode("search");
      setQuery("");
      setHits([]);
      setMessages([]);
      setLoading(false);
      setActiveIdx(0);
      setVoiceState({ kind: "off" });
      return;
    }
    inputRef.current?.focus();
  }, [open]);

  // Lexical search debounce — only runs in search mode.
  useEffect(() => {
    if (!open || mode !== "search") return;
    const trimmed = query.trim();
    if (trimmed.length === 0) {
      setHits([]);
      setLoading(false);
      setActiveIdx(0);
      return;
    }
    const gen = ++queryGen.current;
    setLoading(true);
    const timer = window.setTimeout(async () => {
      try {
        const results = await searchNotes(trimmed, 20);
        if (queryGen.current !== gen) return;
        setHits(results);
        setActiveIdx(0);
      } catch (err) {
        if (queryGen.current !== gen) return;
        console.error("[search] failed:", err);
        setHits([]);
      } finally {
        if (queryGen.current === gen) setLoading(false);
      }
    }, QUERY_DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
  }, [open, mode, query]);

  // Outside-click dismissal. Click *on* the dialog does not bubble here
  // because the dialog stops propagation in its onMouseDown.
  useEffect(() => {
    if (!open) return;
    const onMouseDown = (e: MouseEvent) => {
      const target = e.target as Node | null;
      if (dialogRef.current && target && dialogRef.current.contains(target)) return;
      onClose();
    };
    window.addEventListener("mousedown", onMouseDown);
    return () => window.removeEventListener("mousedown", onMouseDown);
  }, [open, onClose]);

  // Scroll the active search row into view when navigating with arrows.
  useEffect(() => {
    if (mode !== "search") return;
    if (!resultsRef.current) return;
    const row = resultsRef.current.querySelector<HTMLElement>(
      `[data-row-idx="${activeIdx}"]`,
    );
    row?.scrollIntoView({ block: "nearest" });
  }, [activeIdx, hits, mode]);

  // Auto-scroll the conversation as new tokens arrive.
  useEffect(() => {
    if (mode !== "chat") return;
    if (!conversationRef.current) return;
    conversationRef.current.scrollTop = conversationRef.current.scrollHeight;
  }, [mode, messages]);

  // Tauri `ai-stream` subscription. Active only while open and in chat
  // mode (search-only sessions don't need the listener). Cleanup
  // unsubscribes; the listener filters on turn_id so events from a
  // closed palette can't mutate state for a fresh open.
  useEffect(() => {
    if (!open || mode !== "chat") return;
    let unlisten: UnlistenFn | null = null;
    let cancelled = false;
    (async () => {
      const fn = await listen<AiStreamEvent>("ai-stream", (event) => {
        const ev = event.payload;
        setMessages((prev) =>
          prev.map((m) => {
            if (m.role !== "assistant" || m.turnId !== ev.turn_id) return m;
            if (ev.kind === "sources") {
              return { ...m, sources: ev.sources };
            }
            if (ev.kind === "delta") {
              // Append to the trailing text part if there is one,
              // otherwise push a new text part. Crucial for ordering:
              // a tool pill in the middle of streaming text must not
              // get its trailing text appended onto its `value`.
              const last = m.parts[m.parts.length - 1];
              if (last && last.kind === "text") {
                const updated: MessagePart[] = [...m.parts];
                updated[updated.length - 1] = {
                  kind: "text",
                  value: last.value + ev.text,
                };
                return { ...m, parts: updated };
              }
              return {
                ...m,
                parts: [...m.parts, { kind: "text", value: ev.text }],
              };
            }
            if (ev.kind === "tool_use_start") {
              return {
                ...m,
                parts: [
                  ...m.parts,
                  {
                    kind: "tool",
                    toolId: ev.tool_id,
                    name: ev.name,
                    targetN: ev.target_n,
                    targetTitle: ev.target_title,
                    targetLabel: ev.target_label,
                    targetKind: ev.target_kind,
                    status: "running",
                  },
                ],
              };
            }
            if (ev.kind === "tool_use_done") {
              const updated: MessagePart[] = m.parts.map((p) =>
                p.kind === "tool" && p.toolId === ev.tool_id
                  ? { ...p, status: ev.ok ? "ok" : "error" }
                  : p,
              );
              return { ...m, parts: updated };
            }
            if (ev.kind === "done") {
              return { ...m, status: "done" };
            }
            if (ev.kind === "error") {
              return { ...m, status: "error", error: ev.message };
            }
            return m;
          }),
        );
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
  }, [open, mode]);

  // While voice is recording (from a button mousedown), watch for a
  // window-level mouseup to end recording — the mic button itself
  // unmounts as soon as state flips to "recording" (replaced by the
  // VoiceComposer), so its own onMouseUp can't fire. The keyboard
  // shortcut path emits margin:voice-stop directly so it doesn't need
  // this listener.
  useEffect(() => {
    if (voiceState.kind !== "recording") return;
    const onUp = () => void endVoice();
    window.addEventListener("mouseup", onUp);
    return () => window.removeEventListener("mouseup", onUp);
    // endVoice closes over voiceState via React's state updater
    // functions, so ESLint exhaustive-deps would flag it; safe to skip.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [voiceState.kind]);

  const beginVoice = async () => {
    // Idempotent: already-armed presses are no-ops.
    if (voiceState.kind === "recording" || voiceState.kind === "transcribing") {
      return;
    }
    setVoiceState({ kind: "recording" });
    try {
      await startVoiceRecording();
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setVoiceState({ kind: "didnt-catch", message });
    }
  };

  const endVoice = async () => {
    if (voiceState.kind !== "recording") return;
    setVoiceState({ kind: "transcribing" });
    try {
      const r = await stopVoiceRecording();
      if (r.status === "silent") {
        setVoiceState({ kind: "didnt-catch" });
        return;
      }
      if (r.status === "error") {
        setVoiceState({ kind: "didnt-catch", message: r.text });
        return;
      }
      // Append rather than replace so a user who started typing then
      // voiced the rest gets composite input. Trim avoids double spaces
      // when the existing query already ends with whitespace.
      setQuery((prev) => {
        const sep = prev.length > 0 && !prev.endsWith(" ") ? " " : "";
        return prev + sep + r.text;
      });
      setVoiceState({ kind: "off" });
      // Defer focus: the input is being re-enabled, the focus call has
      // to land after React applies the disabled=false update.
      setTimeout(() => inputRef.current?.focus(), 0);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setVoiceState({ kind: "didnt-catch", message });
    }
  };

  // CustomEvent bridge for Home.tsx's space-hold listener. Must be
  // registered on MOUNT (not gated on `open`) because Home.tsx
  // dispatches the event synchronously inside the keydown handler that
  // also calls setPaletteOpen — if registration were gated on open,
  // the listener wouldn't exist yet when the event fires.
  //
  // Refs let the listener always invoke the latest closure of
  // beginVoice/endVoice (which read voiceState etc. directly) without
  // re-registering on every render.
  const beginVoiceRef = useRef(beginVoice);
  const endVoiceRef = useRef(endVoice);
  beginVoiceRef.current = beginVoice;
  endVoiceRef.current = endVoice;
  useEffect(() => {
    const onStart = () => void beginVoiceRef.current();
    const onStop = () => void endVoiceRef.current();
    window.addEventListener("margin:voice-start", onStart);
    window.addEventListener("margin:voice-stop", onStop);
    return () => {
      window.removeEventListener("margin:voice-start", onStart);
      window.removeEventListener("margin:voice-stop", onStop);
    };
  }, []);

  if (!open) return null;

  const isAnyStreaming = messages.some(
    (m) => m.role === "assistant" && m.status === "streaming",
  );

  const submitChatTurn = async (text: string) => {
    const trimmed = text.trim();
    if (trimmed.length === 0 || isAnyStreaming) return;
    const userMsg: ChatMessage = {
      id: cryptoId(),
      role: "user",
      parts: [{ kind: "text", value: trimmed }],
      status: "done",
    };
    // Generate turn_id up-front so it lands on the message before the
    // backend's `Sources` event can arrive at the listener.
    const turnId = cryptoId();
    const assistantMsg: ChatMessage = {
      id: cryptoId(),
      role: "assistant",
      parts: [],
      status: "streaming",
      turnId,
    };
    // History: every prior message at the moment of submit, in send
    // order. Tool parts are dropped — the model only sees prose
    // history. Backend stores history as plain {role, content} strings.
    const history = messages.map((m) => ({
      role: m.role,
      content: joinText(m.parts),
    }));
    setMessages((prev) => [...prev, userMsg, assistantMsg]);
    setQuery("");
    setMode("chat");
    try {
      await askNotesStart(turnId, trimmed, history);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setMessages((prev) =>
        prev.map((m) =>
          m.id === assistantMsg.id
            ? { ...m, status: "error", error: message }
            : m,
        ),
      );
    }
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Escape") {
      e.preventDefault();
      // Esc in voice mode cancels back to the prior search/chat mode
      // rather than closing the whole palette — gives the user a way
      // to back out of a misfired ⇧⌘K.
      if (voiceModeActive) {
        if (voiceState.kind === "recording") {
          // Best-effort: tell the backend to stop and discard the
          // recording. We don't await — the UI returns to off
          // immediately.
          void stopVoiceRecording().catch(() => {});
        }
        setVoiceState({ kind: "off" });
        return;
      }
      onClose();
      return;
    }
    if (mode === "search") {
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
        // Escalate to AI ask.
        e.preventDefault();
        if (query.trim().length > 0) submitChatTurn(query);
        return;
      }
      if (hits.length === 0) return;
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setActiveIdx((i) => Math.min(i + 1, hits.length - 1));
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setActiveIdx((i) => Math.max(i - 1, 0));
      } else if (e.key === "Enter") {
        e.preventDefault();
        const hit = hits[activeIdx];
        if (hit) {
          onOpenNote(hit.note_path);
          onClose();
        }
      }
    } else {
      // chat mode — Enter sends a follow-up turn.
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        if (query.trim().length > 0) submitChatTurn(query);
      }
    }
  };

  const placeholder =
    mode === "chat"
      ? isAnyStreaming
        ? "Waiting for answer…"
        : "Ask a follow-up…"
      : "Search notes — ⌘↵ to ask AI";

  return (
    <div
      className="palette-backdrop"
      role="presentation"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        ref={dialogRef}
        className={"palette-dialog" + (mode === "chat" ? " mode-chat" : " mode-search")}
        role="dialog"
        aria-modal="true"
        aria-label={mode === "chat" ? "Ask AI about your notes" : "Search notes"}
        onKeyDown={onKeyDown}
        onMouseDown={(e) => e.stopPropagation()}
      >
        {voiceModeActive ? (
          <VoiceComposer state={voiceState} />
        ) : (
          <div className="palette-input-row">
            <span className="palette-input-icon" aria-hidden="true">
              {mode === "chat" ? (
                <IconSparkle size={14} sw={1.7} />
              ) : (
                <IconSearch size={14} sw={1.7} />
              )}
            </span>
            <input
              ref={inputRef}
              type="text"
              className="palette-input"
              placeholder={placeholder}
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              disabled={mode === "chat" && isAnyStreaming}
              spellCheck={false}
              autoCorrect="off"
              autoCapitalize="off"
            />
            <button
              type="button"
              className="palette-mic"
              aria-label="Hold to record voice query"
              title="Hold to record (or hold space)"
              // Hold-to-record. mousedown/touchstart begin, the
              // matching up/cancel events end. Pointer events would
              // unify these but onMouseDown / onMouseUp work fine on
              // both desktop platforms we ship to.
              onMouseDown={(e) => {
                e.preventDefault();
                void beginVoice();
              }}
            >
              <IconMic size={13} sw={1.7} />
            </button>
            {mode === "chat" ? (
              <button
                type="button"
                className="palette-send"
                aria-label="Send message"
                disabled={query.trim().length === 0 || isAnyStreaming}
                onClick={() => submitChatTurn(query)}
              >
                <IconArrowRight size={13} sw={1.7} />
              </button>
            ) : (
              <span className="palette-input-kbd">esc</span>
            )}
          </div>
        )}

        {voiceModeActive ? null : mode === "search" ? (
          <SearchResults
            ref={resultsRef}
            query={query}
            hits={hits}
            loading={loading}
            activeIdx={activeIdx}
            onHover={setActiveIdx}
            onPick={(path) => {
              onOpenNote(path);
              onClose();
            }}
          />
        ) : (
          <Conversation
            ref={conversationRef}
            messages={messages}
            onOpenNote={(path) => {
              onOpenNote(path);
              onClose();
            }}
          />
        )}

        <div className="palette-footer">
          {mode === "chat" ? (
            <span>
              <kbd>↵</kbd> send · <kbd>esc</kbd> close
            </span>
          ) : (
            <span>
              <kbd>↵</kbd> open · <kbd>⌘↵</kbd> ask AI · <kbd>esc</kbd> close
            </span>
          )}
        </div>
      </div>
    </div>
  );
}

function VoiceComposer({ state }: { state: VoiceState }) {
  const status = state.kind;
  return (
    <div className={`palette-voice palette-voice-${status}`}>
      {status === "recording" && (
        <>
          <LevelMeter eventName="voice-level" ariaLabel="Voice input level" />
          <div className="palette-voice-status">
            <span className="palette-voice-pulse" aria-hidden="true" />
            Listening — release space to transcribe
          </div>
        </>
      )}
      {status === "transcribing" && (
        <div className="palette-voice-status">
          <span className="palette-voice-spinner" aria-hidden="true" />
          Transcribing…
        </div>
      )}
      {status === "didnt-catch" && (
        <div className="palette-voice-didnt-catch">
          <div>{state.message ?? "Didn't catch that."}</div>
          <div className="palette-voice-hint">
            Hold <kbd>space</kbd> to try again, or press <kbd>esc</kbd> to cancel.
          </div>
        </div>
      )}
    </div>
  );
}

function SearchResults({
  ref,
  query,
  hits,
  loading,
  activeIdx,
  onHover,
  onPick,
}: {
  ref: React.RefObject<HTMLDivElement | null>;
  query: string;
  hits: SearchHit[];
  loading: boolean;
  activeIdx: number;
  onHover: (idx: number) => void;
  onPick: (path: string) => void;
}) {
  return (
    <div className="palette-results" ref={ref} role="listbox">
      {query.trim().length === 0 ? (
        <div className="palette-empty">
          Type to search across all your notes — titles, bodies, and meeting
          transcripts. Press <kbd>⌘↵</kbd> to ask AI instead.
        </div>
      ) : loading && hits.length === 0 ? (
        <div className="palette-empty">Searching…</div>
      ) : hits.length === 0 ? (
        <div className="palette-empty">No matches.</div>
      ) : (
        hits.map((hit, idx) => (
          <ResultRow
            key={hit.note_path + ":" + hit.source}
            hit={hit}
            active={idx === activeIdx}
            idx={idx}
            onMouseEnter={() => onHover(idx)}
            onClick={() => onPick(hit.note_path)}
          />
        ))
      )}
    </div>
  );
}

function Conversation({
  ref,
  messages,
  onOpenNote,
}: {
  ref: React.RefObject<HTMLDivElement | null>;
  messages: ChatMessage[];
  onOpenNote: (path: string) => void;
}) {
  return (
    <div className="palette-conversation" ref={ref}>
      {messages.map((m) => (
        <MessageBubble key={m.id} message={m} onOpenNote={onOpenNote} />
      ))}
    </div>
  );
}

function MessageBubble({
  message,
  onOpenNote,
}: {
  message: ChatMessage;
  onOpenNote: (path: string) => void;
}) {
  if (message.role === "user") {
    return (
      <div className="palette-msg palette-msg-user">
        <div className="palette-msg-bubble">{joinText(message.parts)}</div>
      </div>
    );
  }
  // Assistant
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
  const fullText = joinText(message.parts);
  // Render chips only for labels the model actually cited across all
  // text parts. The full directory + schedule can be hundreds of
  // entries — showing all of them would be a wall of chips. Both `[N]`
  // (notes) and `[E<N>]` (events) match.
  const citedSources = useMemo(() => {
    if (sources.length === 0) return [];
    const cited = new Set<string>();
    const re = /\[(E?\d{1,3})\]/g;
    let m: RegExpExecArray | null;
    while ((m = re.exec(fullText)) !== null) {
      cited.add(m[1]);
    }
    return sources.filter((s) => cited.has(s.label));
  }, [sources, fullText]);

  const isEmptyAndStreaming =
    message.parts.length === 0 && message.status === "streaming";

  return (
    <div className="palette-msg palette-msg-assistant">
      <div className="palette-msg-bubble">
        {isEmptyAndStreaming ? (
          <span className="palette-msg-typing">
            <span></span>
            <span></span>
            <span></span>
          </span>
        ) : (
          message.parts.map((part, i) =>
            part.kind === "text" ? (
              <CitedText
                key={i}
                text={part.value}
                sources={sources}
                onOpenNote={onOpenNote}
              />
            ) : (
              <ToolPill
                key={i}
                part={part}
                onOpen={() => {
                  // Resolve the source by label and open the right
                  // surface — note path for [N], or the linked event
                  // bundle for [E<N>] (creating one if needed via
                  // openOrCreateEventNote from #62).
                  const src = sources.find((s) => s.label === part.targetLabel);
                  if (!src) return;
                  void openSource(src, onOpenNote);
                }}
              />
            ),
          )
        )}
      </div>
      {citedSources.length > 0 && (
        <div className="palette-sources">
          <div className="palette-sources-label">Sources</div>
          <div className="palette-sources-list">
            {citedSources.map((s) => (
              <button
                key={s.label}
                type="button"
                className={`palette-source-chip ${
                  s.kind === "event" ? "is-event" : "is-note"
                }`}
                title={s.title}
                onClick={() => void openSource(s, onOpenNote)}
              >
                <span className="palette-source-num">{s.label}</span>
                <span className="palette-source-title">{s.title}</span>
              </button>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

function ToolPill({
  part,
  onOpen,
}: {
  part: Extract<MessagePart, { kind: "tool" }>;
  onOpen: () => void;
}) {
  const verb =
    part.name === "read_event_details"
      ? "Reading event"
      : part.name === "read_transcript"
      ? "Reading transcript"
      : "Reading";
  // targetLabel is the new field; fall back to targetN for older
  // events that might still be in flight from a stale runner.
  const label = part.targetLabel || `${part.targetN}`;
  const titleAttr = `${part.name}([${label}] ${part.targetTitle})`;
  return (
    <button
      type="button"
      className={`palette-tool-pill status-${part.status}`}
      title={titleAttr}
      onClick={onOpen}
    >
      <span className="palette-tool-icon" aria-hidden="true">
        {part.status === "ok" ? (
          <IconCheck size={11} sw={2} />
        ) : part.status === "error" ? (
          "✗"
        ) : (
          <IconSearch size={11} sw={1.7} />
        )}
      </span>
      <span className="palette-tool-text">
        {verb} <span className="palette-tool-target">[{label}]</span>
        {part.targetTitle ? ` "${part.targetTitle}"` : ""}
        {part.status === "running" ? "…" : ""}
      </span>
    </button>
  );
}

/// Open the surface a source points at. Notes go through `onOpenNote`
/// directly. Events route through `openOrCreateEventNote` (#62) which
/// creates the linked bundle on first click.
async function openSource(
  source: AskSource,
  onOpenNote: (path: string) => void,
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
  }
}

/// Render assistant text as Markdown with `[N]` markers swapped for
/// clickable chips that link to the corresponding source note.
///
/// Approach: replace each `[N]` in the source with a sentinel `<button>`
/// element BEFORE markdown rendering, so markdown-it sees the inline
/// HTML and passes it through untouched. The button carries the
/// citation number in `data-cite-n`; click handling is delegated on the
/// wrapping div so React doesn't have to mount one handler per chip.
///
/// Streaming partial markdown (incomplete `**bold` etc.) is fine —
/// markdown-it just renders the partial state literally until the
/// closing pair arrives in a later delta.
function CitedText({
  text,
  sources,
  onOpenNote,
}: {
  text: string;
  sources: AskSource[];
  onOpenNote: (path: string) => void;
}) {
  const sourceByLabel = useMemo(() => {
    const m = new Map<string, AskSource>();
    for (const s of sources) m.set(s.label, s);
    return m;
  }, [sources]);

  const html = useMemo(() => {
    const withCitations = text.replace(/\[(E?\d{1,3})\]/g, (full, label) => {
      const src = sourceByLabel.get(label);
      // Hallucinated citation label — leave the marker as plain text
      // rather than emitting a dead chip.
      if (!src) return full;
      const variant = src.kind === "event" ? "is-event" : "is-note";
      return `<button type="button" class="palette-cite ${variant}" data-cite-label="${label}">${label}</button>`;
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
    if (source) void openSource(source, onOpenNote);
  };

  return (
    <div
      className="palette-md"
      onClick={onClick}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
}

function ResultRow({
  hit,
  active,
  idx,
  onMouseEnter,
  onClick,
}: {
  hit: SearchHit;
  active: boolean;
  idx: number;
  onMouseEnter: () => void;
  onClick: () => void;
}) {
  return (
    <div
      role="option"
      aria-selected={active}
      data-row-idx={idx}
      className={"palette-row" + (active ? " active" : "")}
      onMouseEnter={onMouseEnter}
      onClick={onClick}
    >
      <span className="palette-row-icon" aria-hidden="true">
        {hit.source === "transcript" ? (
          <IconMic size={13} sw={1.6} />
        ) : (
          <IconFileText size={13} sw={1.6} />
        )}
      </span>
      <span className="palette-row-body">
        <span className="palette-row-title">
          <Highlighted text={hit.title} />
        </span>
        <span className="palette-row-snippet">
          <Highlighted text={hit.snippet} />
        </span>
      </span>
      <span className={"palette-row-tag tag-" + hit.source}>{labelFor(hit.source)}</span>
    </div>
  );
}

function labelFor(source: SearchHit["source"]): string {
  if (source === "title") return "Title";
  if (source === "body") return "Body";
  return "Transcript";
}

function Highlighted({ text }: { text: string }) {
  const parts = useMemo(() => splitHighlights(text), [text]);
  return (
    <>
      {parts.map((p, i) =>
        p.match ? (
          <mark key={i} className="palette-highlight">
            {p.text}
          </mark>
        ) : (
          <span key={i}>{p.text}</span>
        ),
      )}
    </>
  );
}

function splitHighlights(text: string): { text: string; match: boolean }[] {
  const out: { text: string; match: boolean }[] = [];
  let i = 0;
  while (i < text.length) {
    const open = text.indexOf(SEARCH_HIGHLIGHT_OPEN, i);
    if (open === -1) {
      out.push({ text: text.slice(i), match: false });
      break;
    }
    if (open > i) out.push({ text: text.slice(i, open), match: false });
    const close = text.indexOf(SEARCH_HIGHLIGHT_CLOSE, open + 1);
    if (close === -1) {
      out.push({ text: text.slice(open), match: false });
      break;
    }
    out.push({ text: text.slice(open + 1, close), match: true });
    i = close + 1;
  }
  return out;
}

function cryptoId(): string {
  // crypto.randomUUID is available in modern browsers + Tauri webview.
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return Math.random().toString(36).slice(2);
}
