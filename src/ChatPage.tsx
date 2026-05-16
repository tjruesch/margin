//! Full-page chat surface. Same conversation as the cmd+K palette
//! (both read/write the shared `useChat()` context); this one is
//! permanent, scrollable, and primarily for back-and-forth that
//! deserves real screen real estate.

import type React from "react";
import { useCallback, useEffect, useRef, useState } from "react";

import { Conversation } from "./ChatMessage";
import { useChat } from "./ChatProvider";
import { IconArrowRight, IconSparkle } from "./icons";

type Props = {
  onOpenNote: (path: string) => void;
  onOpenWorkstream: (workstreamId: string) => void;
};

export function ChatPage({ onOpenNote, onOpenWorkstream }: Props) {
  const { messages, isStreaming, sendMessage, clearChat } = useChat();
  const [draft, setDraft] = useState("");
  const transcriptRef = useRef<HTMLDivElement | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);

  // Auto-scroll to the latest message on streaming updates.
  useEffect(() => {
    if (!transcriptRef.current) return;
    transcriptRef.current.scrollTop = transcriptRef.current.scrollHeight;
  }, [messages]);

  // Focus the composer on mount.
  useEffect(() => {
    textareaRef.current?.focus();
  }, []);

  const onSubmit = useCallback(async () => {
    const trimmed = draft.trim();
    if (trimmed.length === 0 || isStreaming) return;
    setDraft("");
    await sendMessage(trimmed);
    // Refocus after the textarea re-enables.
    setTimeout(() => textareaRef.current?.focus(), 0);
  }, [draft, isStreaming, sendMessage]);

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey && !e.metaKey && !e.ctrlKey) {
      e.preventDefault();
      void onSubmit();
    }
  };

  const onClear = useCallback(async () => {
    if (messages.length === 0) return;
    await clearChat();
    setTimeout(() => textareaRef.current?.focus(), 0);
  }, [messages.length, clearChat]);

  return (
    <div className="chat-page">
      <header className="chat-page-header">
        <h1 className="chat-page-title">
          <span className="chat-page-title-icon" aria-hidden="true">
            <IconSparkle size={16} sw={1.7} />
          </span>
          Chat
        </h1>
        <button
          type="button"
          className="chat-clear-btn"
          onClick={() => void onClear()}
          disabled={messages.length === 0 || isStreaming}
          title="Archive this conversation and start a new one"
        >
          Clear chat
        </button>
      </header>

      <div className="chat-transcript" ref={transcriptRef}>
        {messages.length === 0 ? (
          <div className="chat-empty-state">
            <h2>Ask Margin</h2>
            <p>
              About your notes, meetings, workstreams, or people. <kbd>⌘K</kbd>
              {" "}opens a quick popover with the same conversation.
            </p>
          </div>
        ) : (
          <Conversation
            messages={messages}
            onOpenNote={onOpenNote}
            onOpenWorkstream={onOpenWorkstream}
          />
        )}
      </div>

      <div className="chat-composer">
        <textarea
          ref={textareaRef}
          className="chat-composer-input"
          placeholder={isStreaming ? "Waiting for answer…" : "Ask anything…"}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={onKeyDown}
          disabled={isStreaming}
          rows={1}
          spellCheck={false}
          autoCorrect="off"
          autoCapitalize="off"
        />
        <button
          type="button"
          className="chat-composer-send"
          aria-label="Send message"
          disabled={draft.trim().length === 0 || isStreaming}
          onClick={() => void onSubmit()}
        >
          <IconArrowRight size={14} sw={1.7} />
        </button>
      </div>
    </div>
  );
}
