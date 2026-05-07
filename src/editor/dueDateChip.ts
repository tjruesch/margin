// CodeMirror plugin that renders trailing `@YYYY-MM-DD[ HH:MM]` tokens on
// checkbox lines as friendly date chips. The chip dissolves back into raw
// text when the cursor enters its line, so the user can edit the token
// without fighting the editor.
//
// Click handling: the chip's DOM is decorated with data attributes for
// `from`/`to` document positions and the parsed `dueMs`. A sibling
// `domEventHandlers` extension catches mousedown on `.cm-due-chip` and
// dispatches a `margin:edit-due` CustomEvent on `document` so a React-
// mounted popover can open at the click coords. The popover writes the
// new token back to the document via the editor's `dispatch`.

import { Range } from "@codemirror/state";
import {
  Decoration,
  DecorationSet,
  EditorView,
  ViewPlugin,
  ViewUpdate,
  WidgetType,
} from "@codemirror/view";

import { dueBucket, friendlyDueLabel, parseAbsoluteToken } from "../dueLabel";

// `- [ ] body @2026-05-15` (with optional ` HH:MM`). The leading-body group
// captures everything up to (but not including) the trailing ` @<token>`
// portion; the second group is the token slice we replace with a chip.
const CHECKBOX_LINE = /^(\s*[-*+]\s+\[[ xX]\]\s)/;
const TRAILING_DUE = /\s+@(\d{4}-\d{2}-\d{2}(?:\s\d{2}:\d{2})?)\s*$/;

class DueChipWidget extends WidgetType {
  constructor(
    readonly dueMs: number,
    readonly tokenFrom: number,
    readonly tokenTo: number,
  ) {
    super();
  }

  toDOM(): HTMLElement {
    const el = document.createElement("span");
    const now = Date.now();
    el.className = `cm-due-chip ${dueBucket(this.dueMs, now)}`;
    el.dataset.from = String(this.tokenFrom);
    el.dataset.to = String(this.tokenTo);
    el.dataset.dueMs = String(this.dueMs);
    el.textContent = friendlyDueLabel(this.dueMs, now);
    el.title = new Date(this.dueMs).toLocaleString();
    return el;
  }

  eq(other: DueChipWidget): boolean {
    return (
      other.dueMs === this.dueMs &&
      other.tokenFrom === this.tokenFrom &&
      other.tokenTo === this.tokenTo
    );
  }

  ignoreEvent(): boolean {
    // Let the click bubble out so domEventHandlers can pick it up.
    return false;
  }
}

function buildDecorations(view: EditorView): DecorationSet {
  const cursor = view.state.selection.main.head;
  const cursorLine = view.state.doc.lineAt(cursor).number;
  const ranges: Range<Decoration>[] = [];

  for (const { from, to } of view.visibleRanges) {
    let pos = from;
    while (pos <= to) {
      const line = view.state.doc.lineAt(pos);
      if (line.number === cursorLine) {
        // Show raw text on the active line so the user can edit naturally.
        pos = line.to + 1;
        continue;
      }
      if (!CHECKBOX_LINE.test(line.text)) {
        pos = line.to + 1;
        continue;
      }
      const match = TRAILING_DUE.exec(line.text);
      if (match) {
        const tokenStart = line.from + match.index + match[0].indexOf("@");
        const tokenEnd = line.from + line.text.trimEnd().length;
        const dueMs = parseAbsoluteToken(match[1]);
        if (dueMs != null) {
          ranges.push(
            Decoration.replace({
              widget: new DueChipWidget(dueMs, tokenStart, tokenEnd),
            }).range(tokenStart, tokenEnd),
          );
        }
      }
      pos = line.to + 1;
    }
  }
  return Decoration.set(ranges, true);
}

export const dueChipPlugin = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet;
    constructor(view: EditorView) {
      this.decorations = buildDecorations(view);
    }
    update(u: ViewUpdate) {
      if (u.docChanged || u.viewportChanged || u.selectionSet) {
        this.decorations = buildDecorations(u.view);
      }
    }
  },
  { decorations: (v) => v.decorations },
);

/** Mousedown handler that intercepts clicks on the chip widget and emits
 *  a `margin:edit-due` CustomEvent the React popover can consume. */
export type EditDueDetail = {
  /** Doc offset of the leading whitespace before `@` (i.e. the start of
   *  the run we replace when the user picks a new date). */
  from: number;
  /** Doc offset just past the last char of the trimmed token. */
  to: number;
  /** Current absolute due ms. */
  dueMs: number;
  /** Viewport-relative coords of the chip for popover positioning. */
  rect: DOMRect;
  /** Editor view, so the popover can dispatch the rewrite transaction. */
  view: EditorView;
};

export const dueChipClickHandler = EditorView.domEventHandlers({
  mousedown(event, view) {
    const target = event.target as HTMLElement | null;
    if (!target || !target.classList.contains("cm-due-chip")) return false;
    const from = Number(target.dataset.from);
    const to = Number(target.dataset.to);
    const dueMs = Number(target.dataset.dueMs);
    if (!Number.isFinite(from) || !Number.isFinite(to) || !Number.isFinite(dueMs)) {
      return false;
    }
    event.preventDefault();
    event.stopPropagation();
    const detail: EditDueDetail = {
      from,
      to,
      dueMs,
      rect: target.getBoundingClientRect(),
      view,
    };
    document.dispatchEvent(new CustomEvent("margin:edit-due", { detail }));
    return true;
  },
});
