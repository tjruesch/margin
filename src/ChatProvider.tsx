//! Shared chat state for the cmd+K palette and the full-page Chat
//! surface. Owns the single source of truth for the active conversation
//! id, the in-memory transcript, the streaming flag, and the lone
//! `ai-stream` Tauri listener. Both consumers call `useChat()`.
//!
//! Persistence model (migration 035):
//! - On mount: `get_active_conversation` (lazy-creates) then
//!   `list_chat_messages` to hydrate the transcript.
//! - On `sendMessage`: append the user row to DB synchronously, then
//!   fire `ask_notes_start`; the `Done` event triggers a write of the
//!   assistant row with accumulated text + sources + tool_calls.
//! - On `clearChat`: archive current, swap to fresh id, drop in-memory
//!   transcript.
//!
//! Stream listener filters by the in-flight `turn_id`, so events for a
//! cleared conversation cannot corrupt the new one.

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import type { ReactNode } from "react";

import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import {
  type AiStreamEvent,
  appendChatMessage,
  askNotesStart,
  type ChatTurn,
  clearActiveConversation,
  getActiveConversation,
  listChatMessages,
  type MessagePart,
} from "./file";
import { joinText, type ChatMessageView } from "./ChatMessage";

type ChatContextValue = {
  conversationId: string | null;
  messages: ChatMessageView[];
  isStreaming: boolean;
  /** Submit a new user turn. Resolves once `ask_notes_start` returns;
   *  streaming and the final-write happen via the `ai-stream` listener.
   *  Caller's UI typically clears its input synchronously, then awaits
   *  this only to surface fatal IPC errors. */
  sendMessage: (text: string) => Promise<void>;
  /** Archive the current conversation and reset in-memory state. */
  clearChat: () => Promise<void>;
};

const ChatContext = createContext<ChatContextValue | null>(null);

function cryptoId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return `${Date.now()}_${Math.random().toString(36).slice(2)}`;
}

/// Hydrate a persisted message row into the in-memory view shape. The
/// DB stores `text` and `tool_calls` as separate columns; we reconstruct
/// a flat `MessagePart[]` ordered text-first so the bubble renders the
/// prose with the recorded tool pills following. Real-time ordering
/// (tool pills interleaved with text deltas) is only visible during the
/// live stream — once a turn is "done" and persisted, the display
/// collapses to "the assistant's prose, with any tools it used."
function hydrateMessage(row: import("./file").ChatMessageRow): ChatMessageView {
  const parts: MessagePart[] = [];
  if (row.text.length > 0) parts.push({ kind: "text", value: row.text });
  for (const tc of row.tool_calls) {
    parts.push({
      kind: "tool",
      toolId: tc.tool_id,
      name: tc.name,
      targetN: 0,
      targetTitle: tc.target_title,
      targetLabel: tc.target_label,
      targetKind: tc.target_kind,
      status: tc.status,
    });
  }
  return {
    id: row.id,
    role: row.role,
    parts,
    sources: row.sources.length > 0 ? row.sources : undefined,
    status: "done",
    turnId: row.turn_id ?? undefined,
  };
}

export function ChatProvider({ children }: { children: ReactNode }) {
  const [conversationId, setConversationId] = useState<string | null>(null);
  const [messages, setMessages] = useState<ChatMessageView[]>([]);
  const messagesRef = useRef<ChatMessageView[]>([]);
  messagesRef.current = messages;
  const conversationIdRef = useRef<string | null>(null);
  conversationIdRef.current = conversationId;
  /** Set of turn_ids belonging to the *current* conversation. After a
   *  clearChat we drop the in-flight turn_id from this set so trailing
   *  stream events from the archived turn cannot mutate the fresh
   *  transcript. */
  const liveTurnIdsRef = useRef<Set<string>>(new Set());

  // Hydrate on mount.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const conv = await getActiveConversation();
        if (cancelled) return;
        setConversationId(conv.id);
        const rows = await listChatMessages(conv.id);
        if (cancelled) return;
        setMessages(rows.map(hydrateMessage));
      } catch (err) {
        console.error("[chat] hydrate failed:", err);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // Single ai-stream listener. Always mounted; the per-turn filter on
  // liveTurnIdsRef keeps stale events from leaking into the current
  // conversation after a clearChat.
  useEffect(() => {
    let unlisten: UnlistenFn | null = null;
    let cancelled = false;
    (async () => {
      const fn = await listen<AiStreamEvent>("ai-stream", (event) => {
        const ev = event.payload;
        if (!liveTurnIdsRef.current.has(ev.turn_id)) return;
        setMessages((prev) =>
          prev.map((m) => {
            if (m.role !== "assistant" || m.turnId !== ev.turn_id) return m;
            if (ev.kind === "sources") {
              return { ...m, sources: ev.sources };
            }
            if (ev.kind === "delta") {
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
        // Persist on terminal events. Read latest message state from
        // the ref (setState above is async; we want the *post*-update
        // version captured by the next tick). Done|error always close
        // the turn — drop it from the live set either way.
        if (ev.kind === "done" || ev.kind === "error") {
          // Defer to next microtask so the setMessages update flushes.
          queueMicrotask(() => {
            const convId = conversationIdRef.current;
            if (!convId) return;
            const finalMsg = messagesRef.current.find(
              (m) => m.role === "assistant" && m.turnId === ev.turn_id,
            );
            liveTurnIdsRef.current.delete(ev.turn_id);
            if (!finalMsg) return;
            if (ev.kind === "error") return; // don't persist failed turns
            const text = joinText(finalMsg.parts);
            const toolCalls = finalMsg.parts
              .filter((p): p is Extract<MessagePart, { kind: "tool" }> => p.kind === "tool")
              .map((p) => ({
                tool_id: p.toolId,
                name: p.name,
                target_label: p.targetLabel,
                target_title: p.targetTitle,
                target_kind: p.targetKind,
                status: p.status,
              }));
            void appendChatMessage(
              convId,
              "assistant",
              text,
              finalMsg.sources,
              toolCalls,
              ev.turn_id,
            ).catch((err) =>
              console.error("[chat] persist assistant failed:", err),
            );
          });
        }
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
  }, []);

  const isStreaming = useMemo(
    () => messages.some((m) => m.role === "assistant" && m.status === "streaming"),
    [messages],
  );

  const sendMessage = useCallback(async (text: string) => {
    const trimmed = text.trim();
    if (trimmed.length === 0) return;
    if (messagesRef.current.some(
      (m) => m.role === "assistant" && m.status === "streaming",
    )) {
      return;
    }
    const convId = conversationIdRef.current;
    if (!convId) {
      console.warn("[chat] sendMessage before conversation hydrated");
      return;
    }
    const turnId = cryptoId();
    const userMsg: ChatMessageView = {
      id: cryptoId(),
      role: "user",
      parts: [{ kind: "text", value: trimmed }],
      status: "done",
    };
    const assistantMsg: ChatMessageView = {
      id: cryptoId(),
      role: "assistant",
      parts: [],
      status: "streaming",
      turnId,
    };
    const history: ChatTurn[] = messagesRef.current.map((m) => ({
      role: m.role,
      content: joinText(m.parts),
    }));
    setMessages((prev) => [...prev, userMsg, assistantMsg]);
    liveTurnIdsRef.current.add(turnId);
    // Persist the user row immediately so a crash mid-stream still
    // keeps the question visible on next launch.
    void appendChatMessage(convId, "user", trimmed).catch((err) =>
      console.error("[chat] persist user failed:", err),
    );
    try {
      await askNotesStart(turnId, trimmed, history);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      liveTurnIdsRef.current.delete(turnId);
      setMessages((prev) =>
        prev.map((m) =>
          m.id === assistantMsg.id ? { ...m, status: "error", error: message } : m,
        ),
      );
    }
  }, []);

  const clearChat = useCallback(async () => {
    try {
      const fresh = await clearActiveConversation();
      // Drop in-flight turn ids so trailing stream events for the
      // archived conversation are silently dropped by the listener.
      liveTurnIdsRef.current.clear();
      setConversationId(fresh.id);
      setMessages([]);
    } catch (err) {
      console.error("[chat] clear failed:", err);
    }
  }, []);

  const value = useMemo<ChatContextValue>(
    () => ({
      conversationId,
      messages,
      isStreaming,
      sendMessage,
      clearChat,
    }),
    [conversationId, messages, isStreaming, sendMessage, clearChat],
  );

  return <ChatContext.Provider value={value}>{children}</ChatContext.Provider>;
}

export function useChat(): ChatContextValue {
  const ctx = useContext(ChatContext);
  if (!ctx) {
    throw new Error("useChat called outside ChatProvider");
  }
  return ctx;
}
