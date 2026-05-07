// Apply the minimal change between `oldText` and `newText` directly to
// the EditorView. Used after Rust rewrites relative due-date tokens
// (`@today` → `@2026-05-08`) on save: dispatching the narrow diff lets
// CodeMirror map the selection across the change instead of collapsing
// it to position 0 the way a full-doc replace would, which kept the
// cursor and scroll position bouncing to the top of the document.

import type { EditorView } from "@codemirror/view";

/** Dispatch a single change covering the differing slice between
 *  `oldText` and `newText` (computed via longest common prefix +
 *  suffix). No-op if the strings are identical. */
export function dispatchDiff(view: EditorView, oldText: string, newText: string): void {
  if (oldText === newText) return;

  const oldLen = oldText.length;
  const newLen = newText.length;
  const minLen = Math.min(oldLen, newLen);

  let prefix = 0;
  while (prefix < minLen && oldText.charCodeAt(prefix) === newText.charCodeAt(prefix)) {
    prefix++;
  }

  let suffix = 0;
  while (
    suffix < minLen - prefix &&
    oldText.charCodeAt(oldLen - 1 - suffix) === newText.charCodeAt(newLen - 1 - suffix)
  ) {
    suffix++;
  }

  const from = prefix;
  const to = oldLen - suffix;
  const insert = newText.slice(prefix, newLen - suffix);
  view.dispatch({ changes: { from, to, insert } });
}
