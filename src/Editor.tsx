import { useMemo } from "react";
import CodeMirror, { EditorView, Extension } from "@uiw/react-codemirror";
import { indentUnit } from "@codemirror/language";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { languages } from "@codemirror/language-data";

type Props = {
  value: string;
  onChange: (next: string) => void;
  tabSize: number;
  useTabs: boolean;
  softWrap: boolean;
  theme: "light" | "dark";
};

export function Editor({ value, onChange, tabSize, useTabs, softWrap, theme }: Props) {
  const extensions = useMemo<Extension[]>(() => {
    const exts: Extension[] = [
      markdown({ base: markdownLanguage, codeLanguages: languages }),
      indentUnit.of(useTabs ? "\t" : " ".repeat(tabSize)),
      EditorView.theme({
        "&": { fontSize: "14px", height: "100%" },
        ".cm-scroller": {
          fontFamily:
            'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
          lineHeight: "1.5",
        },
        ".cm-content": { padding: "16px 0" },
        ".cm-gutters": { backgroundColor: "transparent", border: "none" },
      }),
    ];
    if (softWrap) exts.push(EditorView.lineWrapping);
    return exts;
  }, [softWrap, tabSize, useTabs]);

  return (
    <CodeMirror
      value={value}
      onChange={onChange}
      theme={theme === "dark" ? "dark" : "light"}
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
}
