import { useCallback, useEffect, useRef, useState } from "react";
import {
  type ActionListItem,
  type TeamMember,
  type Workstream,
  deleteAction,
  listActionsForNote,
  setActionAssignee,
  setActionDone,
  setActionWorkstream,
  undoAutoResolvedAction,
} from "./file";
import { IconChevRight } from "./icons";
import { ActionRow } from "./Home";

type Props = {
  notePath: string | null;
  /** Bumped by the parent whenever the saved body changes (reconcile
   *  finished, manual save) so the sidebar refetches. */
  refreshKey: number | string;
  members: TeamMember[];
  workstreams: Workstream[];
  onOpenWorkstream: (id: string) => void;
};

const COLLAPSED_STORAGE_KEY = "note-sidebar-collapsed";

function readCollapsedDefault(): boolean | null {
  if (typeof localStorage === "undefined") return null;
  const v = localStorage.getItem(COLLAPSED_STORAGE_KEY);
  if (v === "1") return true;
  if (v === "0") return false;
  return null;
}

export function NoteActionsSidebar({
  notePath,
  refreshKey,
  members,
  workstreams,
  onOpenWorkstream,
}: Props) {
  const [items, setItems] = useState<ActionListItem[]>([]);
  // Tri-state on first render: respect stored preference if any; otherwise
  // open by default and let the first fetch decide whether to auto-collapse
  // when there are no actions to show.
  const stored = useRef<boolean | null>(readCollapsedDefault());
  const [collapsed, setCollapsed] = useState<boolean>(stored.current ?? false);
  const sawFirstFetchRef = useRef(false);

  // Fetch the per-note action list on note change / refresh signal.
  useEffect(() => {
    if (!notePath) {
      setItems([]);
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const next = await listActionsForNote(notePath);
        if (cancelled) return;
        setItems(next);
        // On the first fetch for this mount, auto-collapse if the user
        // has no stored preference AND there's nothing to show.
        if (!sawFirstFetchRef.current && stored.current === null) {
          sawFirstFetchRef.current = true;
          if (next.length === 0) setCollapsed(true);
        }
      } catch (e) {
        if (!cancelled) console.error("[note-sidebar] list failed:", e);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [notePath, refreshKey]);

  const persistCollapsed = useCallback((next: boolean) => {
    setCollapsed(next);
    stored.current = next;
    if (typeof localStorage !== "undefined") {
      localStorage.setItem(COLLAPSED_STORAGE_KEY, next ? "1" : "0");
    }
  }, []);

  // ---- Mutations (optimistic + refetch on failure) -----------------------

  const refetch = useCallback(async () => {
    if (!notePath) return;
    try {
      const next = await listActionsForNote(notePath);
      setItems(next);
    } catch (e) {
      console.error("[note-sidebar] refetch failed:", e);
    }
  }, [notePath]);

  const onToggle = useCallback(
    (id: string, nextDone: boolean) => {
      setItems((curr) => curr.map((a) => (a.id === id ? { ...a, done: nextDone } : a)));
      void (async () => {
        try {
          await setActionDone(id, nextDone);
        } catch (e) {
          console.error("[note-sidebar] toggle failed:", e);
          void refetch();
        }
      })();
    },
    [refetch],
  );

  const onDelete = useCallback(
    (id: string) => {
      setItems((curr) => curr.filter((a) => a.id !== id));
      void (async () => {
        try {
          await deleteAction(id);
        } catch (e) {
          console.error("[note-sidebar] delete failed:", e);
          void refetch();
        }
      })();
    },
    [refetch],
  );

  const onReassign = useCallback(
    (id: string, memberId: string | null) => {
      const display = memberId
        ? members.find((m) => m.id === memberId)?.display_name ?? null
        : null;
      setItems((curr) =>
        curr.map((a) =>
          a.id === id
            ? { ...a, assignee_id: memberId, assignee_display_name: display }
            : a,
        ),
      );
      void (async () => {
        try {
          await setActionAssignee(id, memberId);
        } catch (e) {
          console.error("[note-sidebar] reassign failed:", e);
          void refetch();
        }
      })();
    },
    [members, refetch],
  );

  const onReattachWorkstream = useCallback(
    (id: string, wsId: string | null) => {
      const title = wsId ? workstreams.find((w) => w.id === wsId)?.title ?? null : null;
      setItems((curr) =>
        curr.map((a) =>
          a.id === id ? { ...a, workstream_id: wsId, workstream_title: title } : a,
        ),
      );
      void (async () => {
        try {
          await setActionWorkstream(id, wsId);
        } catch (e) {
          console.error("[note-sidebar] reattach workstream failed:", e);
          void refetch();
        }
      })();
    },
    [refetch, workstreams],
  );

  const onUndoAutoResolved = useCallback(
    (id: string) => {
      setItems((curr) =>
        curr.map((a) =>
          a.id === id ? { ...a, auto_resolved_ms: null, done: false } : a,
        ),
      );
      void (async () => {
        try {
          await undoAutoResolvedAction(id);
        } catch (e) {
          console.error("[note-sidebar] undo auto-resolved failed:", e);
          void refetch();
        }
      })();
    },
    [refetch],
  );

  return (
    <aside className={`note-sidebar${collapsed ? " collapsed" : ""}`}>
      <button
        type="button"
        className="note-sidebar-toggle"
        onClick={() => persistCollapsed(!collapsed)}
        aria-expanded={!collapsed}
        title={collapsed ? "Show actions" : "Hide actions"}
      >
        <span className={`note-sidebar-chevron${collapsed ? "" : " open"}`}>
          <IconChevRight size={12} sw={2} />
        </span>
        {!collapsed && <span className="note-sidebar-title">Actions ({items.length})</span>}
      </button>
      {!collapsed && (
        <div className="note-sidebar-body">
          {items.length === 0 ? (
            <div className="note-sidebar-empty">No tracked actions for this note.</div>
          ) : (
            items.map((it) => (
              <ActionRow
                key={it.id}
                it={it}
                onToggle={onToggle}
                onDelete={onDelete}
                onOpenNote={() => {}}
                onOpenWorkstream={onOpenWorkstream}
                members={members}
                workstreams={workstreams}
                onReassign={onReassign}
                onReattachWorkstream={onReattachWorkstream}
                onUndoAutoResolved={onUndoAutoResolved}
              />
            ))
          )}
        </div>
      )}
    </aside>
  );
}
