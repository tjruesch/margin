// CodeMirror plugin that renders a small assignee chip at the end of
// each `- [ ] {text}` line. Mirrors `dueDateChip.ts` — same WidgetType +
// ViewPlugin + domEventHandlers shape — but the data flows in via a
// `Facet` since the assignment information lives in the SQLite `actions`
// table, not in the line text.
//
// Click handling: the chip's DOM carries `data-action-id` so the popover
// (assigneePopover.tsx) can dispatch `setActionAssignee` against the
// right row. The mousedown handler emits a `margin:edit-assignee`
// CustomEvent that the popover listens for.

import { Facet, Range } from "@codemirror/state";
import {
  Decoration,
  DecorationSet,
  EditorView,
  ViewPlugin,
  ViewUpdate,
  WidgetType,
} from "@codemirror/view";

import type { ActionListItem } from "../file";
import { avatarColor, initialsFromName } from "../initials";
import { actionTextHash } from "./actionHash";
import { parseActionLine } from "./parseActionLine";

/// Read-only Facet carrying the per-note `Map<textHash, ActionListItem>`
/// from React state into the plugin. The Editor wires this up via
/// `actionsByHash.of(...)`. Combine returns the first non-empty map so
/// extension overrides win deterministically.
export const actionsByHash = Facet.define<
  Map<string, ActionListItem>,
  Map<string, ActionListItem>
>({
  combine: (values) => values[0] ?? new Map(),
});

class AssigneeChipWidget extends WidgetType {
  constructor(
    readonly actionId: string,
    readonly assigneeId: string | null,
    readonly assigneeDisplayName: string | null,
  ) {
    super();
  }

  toDOM(): HTMLElement {
    const el = document.createElement("span");
    el.dataset.actionId = this.actionId;
    if (this.assigneeId && this.assigneeDisplayName) {
      el.className = "cm-assignee-chip";
      el.style.background = avatarColor(this.assigneeId);
      el.textContent = initialsFromName(this.assigneeDisplayName);
      el.title = this.assigneeDisplayName;
    } else {
      el.className = "cm-assignee-chip-empty";
      el.textContent = "+";
      el.title = "Assign…";
    }
    return el;
  }

  eq(other: AssigneeChipWidget): boolean {
    return (
      other.actionId === this.actionId &&
      other.assigneeId === this.assigneeId &&
      other.assigneeDisplayName === this.assigneeDisplayName
    );
  }

  ignoreEvent(): boolean {
    // Let the click bubble out so domEventHandlers can pick it up.
    return false;
  }
}

function buildDecorations(view: EditorView): DecorationSet {
  const map = view.state.facet(actionsByHash);
  if (map.size === 0) return Decoration.none;
  const cursor = view.state.selection.main.head;
  const cursorLine = view.state.doc.lineAt(cursor).number;
  const ranges: Range<Decoration>[] = [];

  for (const { from, to } of view.visibleRanges) {
    let pos = from;
    while (pos <= to) {
      const line = view.state.doc.lineAt(pos);
      if (line.number === cursorLine) {
        // Hide on the cursor line so editing isn't disturbed.
        pos = line.to + 1;
        continue;
      }
      const parsed = parseActionLine(line.text);
      if (parsed === null) {
        pos = line.to + 1;
        continue;
      }
      const action = map.get(actionTextHash(parsed.text));
      if (action === undefined) {
        pos = line.to + 1;
        continue;
      }
      ranges.push(
        Decoration.widget({
          widget: new AssigneeChipWidget(
            action.id,
            action.assignee_id,
            action.assignee_display_name,
          ),
          // `side: 1` places the widget AFTER the line content, so the
          // cursor still lands at the natural end of the line when the
          // user presses End.
          side: 1,
        }).range(line.to, line.to),
      );
      pos = line.to + 1;
    }
  }
  return Decoration.set(ranges, true);
}

export const assigneeChipPlugin = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet;
    constructor(view: EditorView) {
      this.decorations = buildDecorations(view);
    }
    update(u: ViewUpdate) {
      if (
        u.docChanged ||
        u.viewportChanged ||
        u.selectionSet ||
        u.startState.facet(actionsByHash) !== u.state.facet(actionsByHash)
      ) {
        this.decorations = buildDecorations(u.view);
      }
    }
  },
  { decorations: (v) => v.decorations },
);

/** Mousedown handler that intercepts clicks on the chip widget and emits
 *  a `margin:edit-assignee` CustomEvent the React popover can consume. */
export type EditAssigneeDetail = {
  /** The action's id (`bundle_id:hash`) — used by the popover to call
   *  setActionAssignee. */
  actionId: string;
  /** Viewport-relative coords of the chip for popover positioning. */
  rect: DOMRect;
};

export const assigneeChipClickHandler = EditorView.domEventHandlers({
  mousedown(event) {
    const target = event.target as HTMLElement | null;
    if (
      !target ||
      (!target.classList.contains("cm-assignee-chip") &&
        !target.classList.contains("cm-assignee-chip-empty"))
    ) {
      return false;
    }
    const actionId = target.dataset.actionId;
    if (!actionId) return false;
    event.preventDefault();
    event.stopPropagation();
    const detail: EditAssigneeDetail = {
      actionId,
      rect: target.getBoundingClientRect(),
    };
    document.dispatchEvent(new CustomEvent("margin:edit-assignee", { detail }));
    return true;
  },
});
