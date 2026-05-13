import { useEffect, useRef, useState } from "react";

import { IconCheck, IconHelp } from "./icons";

type Props = {
  /** When `false`, render a `?` glyph (unresolved). When `true`, render a
   *  check (resolved). Click toggles. */
  resolved: boolean;
  /** Existing answer text to seed the textarea on a re-resolve. Passed
   *  through verbatim on first open; subsequent edits are local until
   *  the user clicks Resolve. */
  initialAnswer: string | null;
  /** Resolve flow — fires when the user clicks "Resolve" or "Resolve
   *  without answer". `null` answer == no resolved_note column write. */
  onResolve: (answer: string | null) => void;
  /** Reopen flow — fires when the user clicks the check on an
   *  already-resolved row. No popover; just a direct call. */
  onReopen: () => void;
};

/// Inline question-marker chip for the Open Questions surface (#113).
/// Mirrors the AssigneeChip popover idiom: a glyph button that toggles
/// an inline popover with a textarea + two buttons. Resolved rows show
/// a check; clicking it calls `onReopen` directly without a prompt.
export function ResolveQuestionPopover({
  resolved,
  initialAnswer,
  onResolve,
  onReopen,
}: Props) {
  const [open, setOpen] = useState(false);
  const [answer, setAnswer] = useState(initialAnswer ?? "");
  const anchorRef = useRef<HTMLDivElement | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);

  useEffect(() => {
    if (!open) return;
    const onMouseDown = (e: MouseEvent) => {
      const target = e.target as Node | null;
      if (anchorRef.current && target && anchorRef.current.contains(target))
        return;
      setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    window.addEventListener("mousedown", onMouseDown);
    window.addEventListener("keydown", onKey);
    // Focus textarea when the popover opens so the user can type
    // immediately.
    queueMicrotask(() => textareaRef.current?.focus());
    return () => {
      window.removeEventListener("mousedown", onMouseDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [open]);

  // Reset the textarea seed each time we transition to opened. Mirrors
  // how AssigneeChip handles its popover state.
  useEffect(() => {
    if (open) setAnswer(initialAnswer ?? "");
  }, [open, initialAnswer]);

  const handleResolveClick = (withAnswer: boolean) => {
    setOpen(false);
    onResolve(withAnswer ? answer.trim() || null : null);
  };

  const handleGlyphClick = (e: React.MouseEvent) => {
    e.stopPropagation();
    if (resolved) {
      // No prompt; reopen is one-click.
      onReopen();
    } else {
      setOpen((v) => !v);
    }
  };

  return (
    <div
      className="resolve-popover-anchor"
      ref={anchorRef}
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => e.stopPropagation()}
    >
      <button
        type="button"
        className={
          "home-checkbox home-question-checkbox" +
          (resolved ? " resolved" : "")
        }
        aria-label={resolved ? "Mark as open" : "Resolve question"}
        title={resolved ? "Reopen question" : "Resolve…"}
        onClick={handleGlyphClick}
      >
        {resolved ? (
          <IconCheck size={20} sw={3.6} />
        ) : (
          <IconHelp size={14} sw={2} />
        )}
      </button>
      {open && !resolved && (
        <div className="resolve-popover" role="menu">
          <textarea
            ref={textareaRef}
            className="resolve-popover-input"
            placeholder="Answer (optional)"
            rows={3}
            value={answer}
            onChange={(e) => setAnswer(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                e.preventDefault();
                handleResolveClick(true);
              }
            }}
          />
          <div className="resolve-popover-actions">
            <button
              type="button"
              className="resolve-popover-btn"
              onClick={() => handleResolveClick(false)}
            >
              Resolve without answer
            </button>
            <button
              type="button"
              className="resolve-popover-btn resolve-popover-primary"
              onClick={() => handleResolveClick(true)}
              disabled={answer.trim().length === 0}
            >
              Resolve
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
