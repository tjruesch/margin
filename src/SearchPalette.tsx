import { useEffect, useMemo, useRef, useState } from "react";

import {
  searchNotes,
  SEARCH_HIGHLIGHT_CLOSE,
  SEARCH_HIGHLIGHT_OPEN,
  type SearchHit,
} from "./file";
import { IconFileText, IconMic, IconSearch } from "./icons";

type Props = {
  open: boolean;
  onClose: () => void;
  onOpenNote: (path: string) => void;
};

const QUERY_DEBOUNCE_MS = 120;

/// ⌘K command palette. Backed by `search_notes` (FTS over title+body
/// plus per-bundle transcript scan). The palette stays mounted while
/// `open` to preserve input/result state across rapid open/close
/// cycles, but resets state on the open→close edge.
export function SearchPalette({ open, onClose, onOpenNote }: Props) {
  const [query, setQuery] = useState("");
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [loading, setLoading] = useState(false);
  const [activeIdx, setActiveIdx] = useState(0);
  const inputRef = useRef<HTMLInputElement | null>(null);
  const dialogRef = useRef<HTMLDivElement | null>(null);
  const listRef = useRef<HTMLDivElement | null>(null);
  // Generation counter so out-of-order search responses can't overwrite
  // newer results when a slow query resolves after a faster one.
  const queryGen = useRef(0);

  // Reset on close so the next open starts fresh. Focus the input on
  // open so the user can type immediately.
  useEffect(() => {
    if (!open) {
      setQuery("");
      setHits([]);
      setLoading(false);
      setActiveIdx(0);
      return;
    }
    inputRef.current?.focus();
  }, [open]);

  // Debounced search. Empty query clears results without a round-trip.
  useEffect(() => {
    if (!open) return;
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
  }, [open, query]);

  // Outside-click + Escape dismissal. Stop propagation on the dialog
  // itself so the global mousedown listener doesn't fire when the user
  // clicks inside.
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

  // Scroll the active row into view when navigating with arrow keys.
  useEffect(() => {
    if (!listRef.current) return;
    const row = listRef.current.querySelector<HTMLElement>(
      `[data-row-idx="${activeIdx}"]`,
    );
    row?.scrollIntoView({ block: "nearest" });
  }, [activeIdx, hits]);

  if (!open) return null;

  const onKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Escape") {
      e.preventDefault();
      onClose();
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
  };

  return (
    <div
      className="palette-backdrop"
      role="presentation"
      onMouseDown={(e) => {
        // Click on the backdrop dismisses; click on the dialog does
        // not (the dialog stops propagation below).
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        ref={dialogRef}
        className="palette-dialog"
        role="dialog"
        aria-modal="true"
        aria-label="Search notes"
        onKeyDown={onKeyDown}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="palette-input-row">
          <span className="palette-input-icon" aria-hidden="true">
            <IconSearch size={14} sw={1.7} />
          </span>
          <input
            ref={inputRef}
            type="text"
            className="palette-input"
            placeholder="Search notes…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            spellCheck={false}
            autoCorrect="off"
            autoCapitalize="off"
          />
          <span className="palette-input-kbd">esc</span>
        </div>
        <div className="palette-results" ref={listRef} role="listbox">
          {query.trim().length === 0 ? (
            <div className="palette-empty">
              Type to search across all your notes — titles, bodies, and
              meeting transcripts.
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
                onMouseEnter={() => setActiveIdx(idx)}
                onClick={() => {
                  onOpenNote(hit.note_path);
                  onClose();
                }}
              />
            ))
          )}
        </div>
      </div>
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
      <span className={"palette-row-tag tag-" + hit.source}>
        {labelFor(hit.source)}
      </span>
    </div>
  );
}

function labelFor(source: SearchHit["source"]): string {
  if (source === "title") return "Title";
  if (source === "body") return "Body";
  return "Transcript";
}

/// Render text containing the U+2068/U+2069 isolate marks the Rust side
/// uses to delimit the matched span. Falls back to plain text if the
/// marks are absent or unbalanced.
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
      // Unbalanced — render remainder verbatim, marks included.
      out.push({ text: text.slice(open), match: false });
      break;
    }
    out.push({ text: text.slice(open + 1, close), match: true });
    i = close + 1;
  }
  return out;
}
