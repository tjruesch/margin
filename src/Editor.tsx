import { forwardRef, useMemo } from "react";
import CodeMirror, {
  EditorView,
  Extension,
  ReactCodeMirrorRef,
} from "@uiw/react-codemirror";
import { keymap, ViewPlugin } from "@codemirror/view";
import { EditorSelection, Prec } from "@codemirror/state";
import { HighlightStyle, indentUnit, syntaxHighlighting } from "@codemirror/language";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { languages } from "@codemirror/language-data";
import { Tag, styleTags, tags as t } from "@lezer/highlight";

import { dueChipClickHandler, dueChipPlugin } from "./editor/dueDateChip";

// Toggle a markdown wrapper (e.g., `**`, `*`, `~~`) around the current
// selection. Per-range behaviour:
//   - Selection wrapped by markers on both sides (whether they're inside
//     or outside the selection) → unwrap.
//   - Empty selection → insert paired markers, place cursor between.
//   - Otherwise → wrap selection.
function toggleWrap(view: EditorView, marker: string): boolean {
  const len = marker.length;
  const tr = view.state.changeByRange((range) => {
    const text = view.state.sliceDoc(range.from, range.to);
    const before = view.state.sliceDoc(Math.max(0, range.from - len), range.from);
    const after = view.state.sliceDoc(range.to, range.to + len);
    if (before === marker && after === marker) {
      return {
        changes: [
          { from: range.from - len, to: range.from, insert: "" },
          { from: range.to, to: range.to + len, insert: "" },
        ],
        range: EditorSelection.range(range.from - len, range.to - len),
      };
    }
    if (text.startsWith(marker) && text.endsWith(marker) && text.length >= 2 * len) {
      const inner = text.slice(len, text.length - len);
      return {
        changes: { from: range.from, to: range.to, insert: inner },
        range: EditorSelection.range(range.from, range.to - 2 * len),
      };
    }
    if (range.empty) {
      return {
        changes: { from: range.from, insert: marker + marker },
        range: EditorSelection.cursor(range.from + len),
      };
    }
    return {
      changes: { from: range.from, to: range.to, insert: marker + text + marker },
      range: EditorSelection.range(range.from + len, range.to + len),
    };
  });
  view.dispatch(tr);
  return true;
}

// Prec.highest so our Cmd+B / Cmd+I / Cmd+Shift+S beat anything that
// basicSetup's keymaps (search, history, defaults) might claim — Cmd+I
// in particular has a tendency to get captured upstream.
const markdownFormatKeymap = Prec.highest(
  keymap.of([
    { key: "Mod-b", run: (v) => toggleWrap(v, "**"), preventDefault: true },
    { key: "Mod-i", run: (v) => toggleWrap(v, "*"), preventDefault: true },
    { key: "Mod-Shift-s", run: (v) => toggleWrap(v, "~~"), preventDefault: true },
  ]),
);

// Click-to-toggle on `- [ ] task` checkboxes: clicking inside the bracket
// region (`[`, the inner char, or `]`) flips the marker between space
// and `x`. Other clicks still move the cursor as usual.
const TASK_LINE = /^(\s*(?:[-*+])\s+\[)([ xX])(\])/;
const taskCheckboxClickPlugin = ViewPlugin.define(() => ({}), {
  eventHandlers: {
    mousedown(event, view) {
      if (event.button !== 0) return false;
      if (event.metaKey || event.shiftKey || event.altKey || event.ctrlKey) return false;
      const pos = view.posAtCoords({ x: event.clientX, y: event.clientY });
      if (pos == null) return false;
      const line = view.state.doc.lineAt(pos);
      const match = TASK_LINE.exec(line.text);
      if (!match) return false;
      // Position of `[` in the document.
      const bracketOpen = line.from + match[1].length - 1;
      // Position of the inner char (the space or x), and of `]`.
      const innerChar = bracketOpen + 1;
      const bracketClose = bracketOpen + 2;
      // Hit zone: anywhere from `[` to one past `]` (slightly forgiving).
      if (pos < bracketOpen || pos > bracketClose + 1) return false;
      const next = match[2] === " " ? "x" : " ";
      view.dispatch({
        changes: { from: innerChar, to: innerChar + 1, insert: next },
      });
      event.preventDefault();
      return true;
    },
  },
});

// Map Lezer syntax tags onto our --hl-* CSS variables so the editor's
// in-line syntax colors track the active theme.
//
// Markdown vocabulary mirrors the polished design:
//   - Heading TEXT in plain ink + weight (no font-size jump — keeps the
//     monospace rhythm). The `#`/`##` MARKER alone is muted blue.
//   - List bullets ('- ') in rust accent (the "marker" color).
//   - List item text inherits --fg with no weight.
//
// `@lezer/markdown` ships a single `t.processingInstruction` tag for both
// `HeaderMark` and `ListMark`, AND wraps every list-item descendant with
// `t.list`. To color them differently and keep the body untouched, we
// rewrite the marks to custom tags via a styleTags extension below.
const headingMarkTag = Tag.define();
const listMarkTag = Tag.define();

const markdownMarkOverrides = {
  props: [
    styleTags({
      HeaderMark: headingMarkTag,
      ListMark: listMarkTag,
    }),
  ],
};

const themedHighlight = HighlightStyle.define([
  { tag: [t.heading1, t.heading2, t.heading3, t.heading4, t.heading5, t.heading6, t.heading], color: "var(--fg)", fontWeight: "600" },
  { tag: headingMarkTag, color: "var(--hl-section)", fontWeight: "600" },
  { tag: listMarkTag, color: "var(--hl-keyword)", fontWeight: "700" },
  { tag: t.strong, color: "var(--hl-keyword)", fontWeight: "bold" },
  { tag: t.emphasis, color: "var(--hl-builtin)", fontStyle: "italic" },
  { tag: t.strikethrough, textDecoration: "line-through" },
  { tag: t.link, color: "var(--accent)", textDecoration: "underline" },
  { tag: t.url, color: "var(--accent)" },
  { tag: t.monospace, color: "var(--hl-string)" },
  { tag: t.quote, color: "var(--hl-comment)", fontStyle: "italic" },

  // Generic code (fenced code blocks pull in their language's own grammar)
  { tag: t.keyword, color: "var(--hl-keyword)" },
  { tag: [t.atom, t.bool, t.special(t.variableName), t.constant(t.variableName)], color: "var(--hl-attr)" },
  { tag: t.number, color: "var(--hl-attr)" },
  { tag: [t.string, t.special(t.string), t.regexp], color: "var(--hl-string)" },
  { tag: [t.literal, t.escape], color: "var(--hl-attr)" },
  { tag: [t.variableName, t.propertyName, t.attributeName], color: "var(--fg)" },
  { tag: [t.function(t.variableName), t.function(t.propertyName)], color: "var(--hl-title)" },
  { tag: [t.typeName, t.className, t.namespace], color: "var(--hl-title)" },
  { tag: [t.tagName, t.angleBracket], color: "var(--hl-tag)" },
  { tag: t.comment, color: "var(--hl-comment)", fontStyle: "italic" },
  { tag: [t.operator, t.derefOperator, t.punctuation, t.bracket, t.separator], color: "var(--fg-muted)" },
  { tag: t.meta, color: "var(--hl-comment)" },
  { tag: [t.self, t.definition(t.variableName)], color: "var(--hl-builtin)" },
  { tag: t.invalid, color: "var(--hl-keyword)" },
]);

type Props = {
  value: string;
  onChange: (next: string) => void;
  tabSize: number;
  useTabs: boolean;
  softWrap: boolean;
  fontSize: number;
};

export const Editor = forwardRef<ReactCodeMirrorRef, Props>(function Editor(
  { value, onChange, tabSize, useTabs, softWrap, fontSize },
  ref,
) {
  const extensions = useMemo<Extension[]>(() => {
    const exts: Extension[] = [
      markdown({
        base: markdownLanguage,
        codeLanguages: languages,
        extensions: [markdownMarkOverrides],
      }),
      indentUnit.of(useTabs ? "\t" : " ".repeat(tabSize)),
      syntaxHighlighting(themedHighlight),
      taskCheckboxClickPlugin,
      dueChipPlugin,
      dueChipClickHandler,
      markdownFormatKeymap,
      EditorView.theme({
        "&": {
          fontSize: `${fontSize}px`,
          height: "100%",
          backgroundColor: "var(--bg)",
          color: "var(--fg)",
        },
        // Center the editing surface in an 880px column on wide screens
        // (matches the polished design's markdown-source page width). The
        // padding collapses to 0 on narrow panes via max(0, ...).
        ".cm-scroller": {
          fontFamily: '"JetBrains Mono Variable", ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
          lineHeight: "1.7",
          letterSpacing: "-0.005em",
          backgroundColor: "var(--bg)",
          paddingLeft: "max(0px, calc((100% - 880px) / 2))",
          paddingRight: "max(0px, calc((100% - 880px) / 2))",
        },
        ".cm-content": {
          padding: "20px 0 64px",
          caretColor: "var(--fg)",
        },
        ".cm-line": {
          paddingLeft: "22px",
          paddingRight: "28px",
        },
        // Quiet gutter channel: small tabular numbers, hairline separator.
        ".cm-gutters": {
          backgroundColor: "transparent",
          color: "var(--fg-muted)",
          border: "none",
          borderRight: "0.5px solid var(--border-muted)",
        },
        ".cm-lineNumbers .cm-gutterElement": {
          fontSize: "12px",
          fontVariantNumeric: "tabular-nums",
          padding: "0 12px 0 14px",
          minWidth: "30px",
        },
        ".cm-activeLine": {
          // Subtle row tint — mirrors the polished design's hover/active
          // affordance without competing with the selection.
          backgroundColor: "color-mix(in srgb, var(--fg) 2.5%, transparent)",
          // Accent stripe on the active line's left edge.
          boxShadow: "inset 2px 0 0 var(--accent)",
        },
        ".cm-activeLineGutter": {
          backgroundColor: "transparent",
          color: "var(--fg)",
          fontWeight: "600",
        },
        ".cm-cursor, .cm-dropCursor": {
          borderLeftColor: "var(--fg)",
        },
        // Selection styling lives in App.css — see `.cm-editor .cm-selectionBackground`
        // there. Inline EditorView.theme rules lose to CodeMirror's built-in
        // selection layer styles in some browsers, so we hoist it out and use
        // a strong selector + !important.
        ".cm-foldPlaceholder": {
          backgroundColor: "var(--bg-muted)",
          color: "var(--fg-muted)",
          border: "1px solid var(--border)",
        },
      }),
    ];
    if (softWrap) exts.push(EditorView.lineWrapping);
    return exts;
  }, [softWrap, tabSize, useTabs, fontSize]);

  return (
    <CodeMirror
      ref={ref}
      value={value}
      onChange={onChange}
      theme="none"
      extensions={extensions}
      basicSetup={{
        lineNumbers: true,
        highlightActiveLine: true,
        highlightActiveLineGutter: true,
        foldGutter: true,
        bracketMatching: true,
        closeBrackets: true,
        autocompletion: false,
        tabSize,
        indentOnInput: true,
      }}
      indentWithTab
      style={{ height: "100%" }}
      height="100%"
    />
  );
});
