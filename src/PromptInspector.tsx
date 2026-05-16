//! Modal that surfaces "what did the AI see for this turn?" — the
//! full assembled prompt, the tool-spec the model had available, every
//! tool dispatch (with truncated input + output), the source list, and
//! a citations-check that flags labels the model emitted that weren't
//! in the source surface (hallucinated). Triggered from the 🔍 button
//! on assistant message bubbles (#134).

import { useEffect, useMemo, useState } from "react";
import type React from "react";

import { getPromptDump, type PromptDispatch, type PromptDump } from "./file";
import { joinText, type ChatMessageView, chipVariant } from "./ChatMessage";

type Props = {
  message: ChatMessageView;
  onClose: () => void;
};

export function PromptInspector({ message, onClose }: Props) {
  const [dump, setDump] = useState<PromptDump | null>(null);
  const [loading, setLoading] = useState(true);

  // Hydrate the dump on mount; refetch when the user clicks 🔍 on a
  // different message (PromptInspector unmounts + remounts then).
  useEffect(() => {
    if (!message.turnId) {
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    void getPromptDump(message.turnId).then((d) => {
      if (cancelled) return;
      setDump(d);
      setLoading(false);
    });
    return () => {
      cancelled = true;
    };
  }, [message.turnId]);

  // Escape closes the modal. Listen on window because focus may be
  // outside the dialog (e.g. user scrolled with no focused control).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div className="inspector-backdrop" role="presentation" onClick={onClose}>
      <div
        className="inspector-card"
        role="dialog"
        aria-modal="true"
        aria-label="Prompt inspector"
        onClick={(e) => e.stopPropagation()}
      >
        <header className="inspector-header">
          <div className="inspector-title">
            Prompt inspector
            {message.turnId && (
              <span className="inspector-turn-id" title={message.turnId}>
                · {message.turnId.slice(0, 8)}
              </span>
            )}
          </div>
          <button
            type="button"
            className="inspector-close"
            aria-label="Close inspector"
            onClick={onClose}
          >
            ×
          </button>
        </header>

        {loading ? (
          <div className="inspector-empty">Loading…</div>
        ) : !dump ? (
          <div className="inspector-empty">
            No prompt dump recorded for this turn.
            {!message.turnId && " (Pre-#134 history.)"}
          </div>
        ) : (
          <InspectorBody dump={dump} message={message} />
        )}
      </div>
    </div>
  );
}

function InspectorBody({
  dump,
  message,
}: {
  dump: PromptDump;
  message: ChatMessageView;
}) {
  // Citations the model actually emitted in its prose. Same regex as
  // ChatMessage.tsx — handles [N], [E<N>], [W<N>], [T<N>].
  const emittedLabels = useMemo(() => {
    const text = joinText(message.parts);
    const out = new Set<string>();
    const re = /\[([WET]?\d{1,3})\]/g;
    let m: RegExpExecArray | null;
    while ((m = re.exec(text)) !== null) out.add(m[1]);
    return Array.from(out);
  }, [message.parts]);

  const sourceLabels = useMemo(() => {
    const out = new Set<string>();
    for (const s of dump.sources) out.add(s.label);
    return out;
  }, [dump.sources]);

  const hallucinated = emittedLabels.filter((l) => !sourceLabels.has(l));

  return (
    <div className="inspector-body">
      <MetaRow dump={dump} />

      <Section title={`Citations (${emittedLabels.length})`} defaultOpen={true}>
        {emittedLabels.length === 0 ? (
          <div className="inspector-empty-row">
            The model didn't cite any sources in this answer.
          </div>
        ) : (
          <ul className="inspector-citations">
            {emittedLabels.map((label) => {
              const source = dump.sources.find((s) => s.label === label);
              const isHallucinated = !source;
              return (
                <li
                  key={label}
                  className={
                    "inspector-citation" +
                    (isHallucinated ? " inspector-citation-hallucinated" : "")
                  }
                  title={
                    isHallucinated
                      ? "Hallucinated: this label was not in the source surface."
                      : source.title
                  }
                >
                  <span
                    className={
                      "palette-source-num " +
                      (source ? chipVariant(source.kind) : "")
                    }
                  >
                    {label}
                  </span>
                  <span className="inspector-citation-title">
                    {isHallucinated ? "(not in sources)" : source.title}
                  </span>
                </li>
              );
            })}
          </ul>
        )}
        {hallucinated.length > 0 && (
          <div className="inspector-hallucinated-note">
            {hallucinated.length} citation{hallucinated.length === 1 ? "" : "s"} the
            model invented (highlighted in red).
          </div>
        )}
      </Section>

      <Section
        title={`Tool dispatches (${dump.dispatches.length})`}
        defaultOpen={dump.dispatches.length > 0}
      >
        {dump.dispatches.length === 0 ? (
          <div className="inspector-empty-row">No tool calls this turn.</div>
        ) : (
          <ul className="inspector-dispatches">
            {dump.dispatches.map((d, i) => (
              <DispatchRow key={i} dispatch={d} />
            ))}
          </ul>
        )}
      </Section>

      <Section
        title={`Sources in the surface (${dump.sources.length})`}
        defaultOpen={false}
      >
        <ul className="inspector-sources">
          {dump.sources.map((s) => (
            <li key={`${s.kind}-${s.label}`}>
              <span
                className={`palette-source-num ${chipVariant(s.kind)}`}
                title={s.kind}
              >
                {s.label}
              </span>
              <span className="inspector-source-title">{s.title}</span>
            </li>
          ))}
        </ul>
      </Section>

      <Section title="Tools available" defaultOpen={false}>
        <ul className="inspector-tool-names">
          {dump.tool_names.map((n) => (
            <li key={n}>
              <code>{n}</code>
            </li>
          ))}
        </ul>
      </Section>

      <Section title="System prompt" defaultOpen={false}>
        <pre className="inspector-prompt-block">{dump.system_prompt}</pre>
      </Section>

      <Section title="User prompt (assembled)" defaultOpen={false}>
        <PromptBlock prompt={dump.prompt} />
      </Section>
    </div>
  );
}

function MetaRow({ dump }: { dump: PromptDump }) {
  const created = new Date(dump.created_ms).toLocaleString();
  return (
    <div className="inspector-meta">
      <span>
        <strong>{dump.latency_ms.toLocaleString()}</strong> ms
      </span>
      <span className="inspector-meta-dot">·</span>
      <span>
        <strong>{dump.sources.length}</strong> sources
      </span>
      <span className="inspector-meta-dot">·</span>
      <span>
        <strong>{dump.dispatches.length}</strong> tool calls
      </span>
      <span className="inspector-meta-dot">·</span>
      <span className="inspector-meta-time" title={created}>
        {created}
      </span>
    </div>
  );
}

function DispatchRow({ dispatch }: { dispatch: PromptDispatch }) {
  const [expanded, setExpanded] = useState(false);
  const preview = dispatch.content.split("\n", 1)[0] ?? "";
  return (
    <li
      className={
        "inspector-dispatch" +
        (dispatch.is_error ? " inspector-dispatch-error" : "")
      }
    >
      <button
        type="button"
        className="inspector-dispatch-head"
        aria-expanded={expanded}
        onClick={() => setExpanded((v) => !v)}
      >
        <span className="inspector-dispatch-chev">{expanded ? "▾" : "▸"}</span>
        <code className="inspector-dispatch-name">{dispatch.tool_name}</code>
        <span className="inspector-dispatch-input">
          {JSON.stringify(dispatch.input)}
        </span>
        <span className="inspector-dispatch-duration">{dispatch.duration_ms}ms</span>
        {!expanded && (
          <span className="inspector-dispatch-preview">→ {preview}</span>
        )}
      </button>
      {expanded && (
        <pre className="inspector-dispatch-body">{dispatch.content}</pre>
      )}
    </li>
  );
}

function PromptBlock({ prompt }: { prompt: string }) {
  // Extract `# Heading` lines for a quick TOC strip. The prompt is
  // a few hundred lines of markdown-ish text; the headings let the
  // user jump to the section they want to read.
  const headings = useMemo(() => {
    const out: { title: string; offset: number }[] = [];
    const lines = prompt.split("\n");
    let offset = 0;
    for (const line of lines) {
      if (line.startsWith("# ")) {
        out.push({ title: line.slice(2).trim(), offset });
      }
      offset += line.length + 1;
    }
    return out;
  }, [prompt]);

  const onJump = (e: React.MouseEvent<HTMLButtonElement>, offset: number) => {
    e.preventDefault();
    // Cheap heuristic for scroll-to-section: count newlines before
    // `offset`, multiply by computed line-height, set scrollTop.
    // Wrapped lines visually take more space but this gets within a
    // few lines of the target, which is fine for a "jump-near" UX.
    const pre = (e.currentTarget.closest(".inspector-section") || document)
      .querySelector<HTMLPreElement>(".inspector-prompt-block");
    if (!pre) return;
    const before = prompt.slice(0, offset);
    const line = (before.match(/\n/g) || []).length;
    const lineHeight = parseFloat(getComputedStyle(pre).lineHeight) || 18;
    pre.scrollTop = Math.max(0, line * lineHeight - 8);
  };

  return (
    <div className="inspector-prompt-wrap">
      {headings.length > 0 && (
        <div className="inspector-toc" role="navigation" aria-label="Prompt sections">
          {headings.map((h, i) => (
            <button
              key={i}
              type="button"
              className="inspector-toc-link"
              onClick={(e) => onJump(e, h.offset)}
            >
              {h.title}
            </button>
          ))}
        </div>
      )}
      <pre className="inspector-prompt-block">{prompt}</pre>
    </div>
  );
}

function Section({
  title,
  defaultOpen,
  children,
}: {
  title: string;
  defaultOpen: boolean;
  children: React.ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <section className="inspector-section">
      <button
        type="button"
        className="inspector-section-head"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
      >
        <span className="inspector-section-chev">{open ? "▾" : "▸"}</span>
        {title}
      </button>
      {open && <div className="inspector-section-body">{children}</div>}
    </section>
  );
}
