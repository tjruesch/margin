import { useEffect, useRef, useState } from "react";

import {
  searchNotes,
  SEARCH_HIGHLIGHT_CLOSE,
  SEARCH_HIGHLIGHT_OPEN,
  type SearchHit,
  startVoiceRecording,
  stopVoiceRecording,
} from "./file";
import { LevelMeter } from "./LevelMeter";
import {
  IconArrowRight,
  IconFileText,
  IconMic,
  IconSearch,
  IconSparkle,
} from "./icons";
import { Conversation } from "./ChatMessage";
import { useChat } from "./ChatProvider";

type Props = {
  open: boolean;
  onClose: () => void;
  onOpenNote: (path: string) => void;
  /** Click-through for `[W*]` chips — switches the sidebar nav to
   *  Workstreams and opens the specified workstream's detail view.
   *  Provided by Home.tsx so it can flip its nav state directly. */
  onOpenWorkstream: (workstreamId: string) => void;
};

const QUERY_DEBOUNCE_MS = 120;

type VoiceState =
  | { kind: "off" }
  | { kind: "recording" }
  | { kind: "transcribing" }
  | { kind: "didnt-catch"; message?: string };

/// Search palette + AI Q&A surface (#31).
///
/// Two modes share the same dialog:
///   - "search": debounced lexical search via `search_notes`. Plain Enter
///     opens the active row.
///   - "chat":  conversation thread streamed from `ask_notes_start`.
///     Plain Enter submits the next turn.
///
/// Cmd+Enter from search escalates to chat (sends the current input as
/// the first user turn). Esc closes and resets the search-mode UI
/// state, but the chat transcript persists across opens via
/// `useChat()` — the same conversation is rendered on the dedicated
/// Chat page.
export function SearchPalette({ open, onClose, onOpenNote, onOpenWorkstream }: Props) {
  const [mode, setMode] = useState<"search" | "chat">("search");
  const [query, setQuery] = useState("");
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [loading, setLoading] = useState(false);
  const [activeIdx, setActiveIdx] = useState(0);
  const [voiceState, setVoiceState] = useState<VoiceState>({ kind: "off" });
  const voiceModeActive = voiceState.kind !== "off";

  const { messages, isStreaming, sendMessage } = useChat();

  const inputRef = useRef<HTMLInputElement | null>(null);
  const dialogRef = useRef<HTMLDivElement | null>(null);
  const resultsRef = useRef<HTMLDivElement | null>(null);
  const conversationRef = useRef<HTMLDivElement | null>(null);
  // Generation counter for the search debounce — out-of-order responses
  // from a slower keystroke must not overwrite a newer one's results.
  const queryGen = useRef(0);

  // Reset search-mode UI state on close. Chat transcript lives in the
  // provider and intentionally survives open/close cycles (#chat-page).
  useEffect(() => {
    if (!open) {
      setMode("search");
      setQuery("");
      setHits([]);
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

  // Window-level Escape handler. The dialog's onKeyDown also handles
  // Escape, but only when focus is inside the dialog. When voice mode
  // is active the <input> is unmounted (replaced by VoiceComposer)
  // and focus falls back to <body>, so dialog-level keydown stops
  // firing and Esc would silently no-op. Listening on window covers
  // every focus state.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      if (voiceState.kind === "recording") {
        void stopVoiceRecording().catch(() => {});
      }
      if (voiceState.kind !== "off") {
        setVoiceState({ kind: "off" });
        return;
      }
      onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose, voiceState.kind]);

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
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [voiceState.kind]);

  const beginVoice = async () => {
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
      setQuery((prev) => {
        const sep = prev.length > 0 && !prev.endsWith(" ") ? " " : "";
        return prev + sep + r.text;
      });
      setVoiceState({ kind: "off" });
      setTimeout(() => inputRef.current?.focus(), 0);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setVoiceState({ kind: "didnt-catch", message });
    }
  };

  // CustomEvent bridge for Home.tsx's space-hold listener. Must be
  // registered on MOUNT (not gated on `open`).
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

  const submitChatTurn = async (text: string) => {
    const trimmed = text.trim();
    if (trimmed.length === 0 || isStreaming) return;
    setQuery("");
    setMode("chat");
    await sendMessage(trimmed);
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Escape") {
      e.preventDefault();
      if (voiceModeActive) {
        if (voiceState.kind === "recording") {
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
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        if (query.trim().length > 0) submitChatTurn(query);
      }
    }
  };

  const placeholder =
    mode === "chat"
      ? isStreaming
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
              disabled={mode === "chat" && isStreaming}
              spellCheck={false}
              autoCorrect="off"
              autoCapitalize="off"
            />
            <button
              type="button"
              className="palette-mic"
              aria-label="Hold to record voice query"
              title="Hold to record (or hold space)"
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
                disabled={query.trim().length === 0 || isStreaming}
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
            onOpenWorkstream={(id) => {
              onOpenWorkstream(id);
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
    <button
      type="button"
      className={"palette-result" + (active ? " is-active" : "")}
      data-row-idx={idx}
      onMouseEnter={onMouseEnter}
      onClick={onClick}
    >
      <span className="palette-result-icon" aria-hidden="true">
        <IconFileText size={14} sw={1.7} />
      </span>
      <span className="palette-result-body">
        <span className="palette-result-title">{hit.title || hit.note_path}</span>
        <span className="palette-result-snippet">
          <Highlighted text={hit.snippet || ""} />
        </span>
      </span>
      <span className="palette-result-source">{labelFor(hit.source)}</span>
    </button>
  );
}

function labelFor(source: SearchHit["source"]): string {
  if (source === "body") return "body";
  if (source === "title") return "title";
  return "transcript";
}

function Highlighted({ text }: { text: string }) {
  const parts = splitHighlights(text);
  return (
    <>
      {parts.map((p, i) =>
        p.match ? <mark key={i}>{p.text}</mark> : <span key={i}>{p.text}</span>,
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
    if (open > i) {
      out.push({ text: text.slice(i, open), match: false });
    }
    const close = text.indexOf(SEARCH_HIGHLIGHT_CLOSE, open + SEARCH_HIGHLIGHT_OPEN.length);
    if (close === -1) {
      out.push({ text: text.slice(open + SEARCH_HIGHLIGHT_OPEN.length), match: true });
      break;
    }
    out.push({
      text: text.slice(open + SEARCH_HIGHLIGHT_OPEN.length, close),
      match: true,
    });
    i = close + SEARCH_HIGHLIGHT_CLOSE.length;
  }
  return out;
}
