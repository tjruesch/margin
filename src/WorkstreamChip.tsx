import { useEffect, useMemo, useRef, useState } from "react";

import { type Workstream } from "./file";
import { IconBriefcase, IconPlus } from "./icons";

type Props = {
  workstreamId: string | null;
  workstreamTitle: string | null;
  workstreams: Workstream[];
  onPick: (workstreamId: string | null) => void;
};

/// Inline workstream-attachment chip for action rows (#111). Mirrors
/// `AssigneeChip`'s popover idiom: a colored chip carrying the current
/// workstream title when attached, a plus-icon button when unattached.
/// Clicking opens an inline popover listing active workstreams plus a
/// "Detach" row when something is currently set.
///
/// The full-screen `WorkstreamPickerModal` from `Workstreams.tsx` is the
/// wrong feel for a per-row chip — that one is a palette for one-off
/// curation. This is a quick toggle wired alongside the assignee chip.
export function WorkstreamChip({
  workstreamId,
  workstreamTitle,
  workstreams,
  onPick,
}: Props) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const anchorRef = useRef<HTMLDivElement | null>(null);

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
    return () => {
      window.removeEventListener("mousedown", onMouseDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [open]);

  useEffect(() => {
    if (!open) setQuery("");
  }, [open]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    const sorted = [...workstreams].sort((a, b) =>
      a.title.localeCompare(b.title, undefined, { sensitivity: "base" }),
    );
    if (!q) return sorted;
    return sorted.filter((w) => w.title.toLowerCase().includes(q));
  }, [workstreams, query]);

  const handlePick = (id: string | null) => {
    setOpen(false);
    onPick(id);
  };

  return (
    <div
      className="workstream-chip-anchor"
      ref={anchorRef}
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => e.stopPropagation()}
    >
      {workstreamId && workstreamTitle ? (
        <button
          type="button"
          className="workstream-chip"
          aria-label={`Attached to ${workstreamTitle}; click to change`}
          aria-haspopup="menu"
          aria-expanded={open}
          title={workstreamTitle}
          onClick={() => setOpen((v) => !v)}
        >
          <IconBriefcase size={11} sw={1.7} />
          <span className="workstream-chip-label">{workstreamTitle}</span>
        </button>
      ) : (
        <button
          type="button"
          className="workstream-chip-empty"
          aria-label="Not attached to a workstream; click to attach"
          aria-haspopup="menu"
          aria-expanded={open}
          title="Attach to workstream…"
          onClick={() => setOpen((v) => !v)}
        >
          <IconBriefcase size={11} sw={1.7} />
          <IconPlus size={9} sw={2} />
        </button>
      )}
      {open && (
        <div className="workstream-chip-popover" role="menu">
          <input
            type="text"
            className="workstream-chip-search"
            placeholder="Search workstreams…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            autoFocus
          />
          <div className="workstream-chip-list">
            {filtered.length === 0 ? (
              <div className="workstream-chip-empty-state">
                No matching workstreams.
              </div>
            ) : (
              filtered.map((w) => {
                const active = w.id === workstreamId;
                return (
                  <button
                    key={w.id}
                    type="button"
                    role="menuitem"
                    className={
                      "workstream-chip-row" + (active ? " active" : "")
                    }
                    onClick={() => handlePick(w.id)}
                  >
                    <IconBriefcase size={11} sw={1.7} />
                    <span className="workstream-chip-row-title">{w.title}</span>
                  </button>
                );
              })
            )}
          </div>
          {workstreamId && (
            <>
              <div className="workstream-chip-sep" />
              <button
                type="button"
                role="menuitem"
                className="workstream-chip-row workstream-chip-detach"
                onClick={() => handlePick(null)}
              >
                Detach from workstream
              </button>
            </>
          )}
        </div>
      )}
    </div>
  );
}
