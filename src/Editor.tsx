import { forwardRef, useMemo } from "react";
import CodeMirror, {
  EditorView,
  Extension,
  ReactCodeMirrorRef,
} from "@uiw/react-codemirror";
import { HighlightStyle, indentUnit, syntaxHighlighting } from "@codemirror/language";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { languages } from "@codemirror/language-data";
import { tags as t } from "@lezer/highlight";

// Map Lezer syntax tags onto our --hl-* CSS variables so the editor's
// in-line syntax colors track the active theme.
const themedHighlight = HighlightStyle.define([
  // Markdown structure
  { tag: [t.heading, t.heading1, t.heading2, t.heading3, t.heading4, t.heading5, t.heading6], color: "var(--hl-section)", fontWeight: "bold" },
  { tag: t.strong, color: "var(--hl-keyword)", fontWeight: "bold" },
  { tag: t.emphasis, color: "var(--hl-builtin)", fontStyle: "italic" },
  { tag: t.strikethrough, textDecoration: "line-through" },
  { tag: t.link, color: "var(--accent)", textDecoration: "underline" },
  { tag: t.url, color: "var(--accent)" },
  { tag: t.monospace, color: "var(--hl-string)" },
  { tag: t.quote, color: "var(--hl-comment)", fontStyle: "italic" },
  { tag: [t.list, t.processingInstruction], color: "var(--hl-tag)" },

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
};

export const Editor = forwardRef<ReactCodeMirrorRef, Props>(function Editor(
  { value, onChange, tabSize, useTabs, softWrap },
  ref,
) {
  const extensions = useMemo<Extension[]>(() => {
    const exts: Extension[] = [
      markdown({ base: markdownLanguage, codeLanguages: languages }),
      indentUnit.of(useTabs ? "\t" : " ".repeat(tabSize)),
      syntaxHighlighting(themedHighlight),
      EditorView.theme({
        "&": {
          fontSize: "14px",
          height: "100%",
          backgroundColor: "var(--bg)",
          color: "var(--fg)",
        },
        ".cm-scroller": {
          fontFamily:
            'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
          lineHeight: "1.5",
          backgroundColor: "var(--bg)",
        },
        ".cm-content": {
          padding: "16px 0",
          caretColor: "var(--fg)",
        },
        ".cm-gutters": {
          backgroundColor: "var(--bg)",
          color: "var(--fg-muted)",
          border: "none",
        },
        ".cm-activeLine": {
          // Translucent overlay so the selection (drawn beneath) stays visible.
          backgroundColor: "color-mix(in srgb, var(--fg) 8%, transparent)",
        },
        ".cm-activeLineGutter": {
          backgroundColor: "color-mix(in srgb, var(--fg) 10%, transparent)",
          color: "var(--fg)",
        },
        ".cm-cursor, .cm-dropCursor": {
          borderLeftColor: "var(--fg)",
        },
        "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection":
          {
            backgroundColor: "color-mix(in srgb, var(--accent) 35%, transparent)",
          },
        ".cm-foldPlaceholder": {
          backgroundColor: "var(--bg-muted)",
          color: "var(--fg-muted)",
          border: "1px solid var(--border)",
        },
      }),
    ];
    if (softWrap) exts.push(EditorView.lineWrapping);
    return exts;
  }, [softWrap, tabSize, useTabs]);

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
