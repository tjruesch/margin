import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { Editor } from "./Editor";
import { Preview } from "./Preview";
import {
  getInitialFile,
  pickFileToOpen,
  pickFileToSave,
  readFile,
  writeFile,
} from "./file";
import "./App.css";

type Mode = "edit" | "preview";

const WELCOME = `# Welcome to Markpad

A lightweight, GitHub-flavored Markdown editor for macOS.

- **Cmd+O** — Open a file
- **Cmd+S** — Save
- **Cmd+Shift+S** — Save As
- **Cmd+E** / **Cmd+P** — Toggle Edit / Preview

## Try it

Edit this text on the left, see the rendered output on the right.

\`\`\`ts
function greet(name: string) {
  return \`Hello, \${name}!\`;
}
\`\`\`

- [x] Renders task lists
- [ ] You can check this in the source

> Blockquotes, tables, footnotes[^1], and emoji :rocket: all work.

[^1]: Like this one.
`;

export default function App() {
  const [mode, setMode] = useState<Mode>("edit");
  const [content, setContent] = useState<string>(WELCOME);
  const [path, setPath] = useState<string | null>(null);
  const [savedContent, setSavedContent] = useState<string>(WELCOME);
  const [tabSize, setTabSize] = useState<number>(2);
  const [useTabs, setUseTabs] = useState<boolean>(false);
  const [softWrap, setSoftWrap] = useState<boolean>(true);
  const [theme, setTheme] = useState<"light" | "dark">(() =>
    window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light",
  );

  const dirty = content !== savedContent;
  const fileName = path ? path.split("/").pop() ?? "Untitled.md" : "Untitled.md";

  const contentRef = useRef(content);
  const pathRef = useRef(path);
  useEffect(() => {
    contentRef.current = content;
  }, [content]);
  useEffect(() => {
    pathRef.current = path;
  }, [path]);

  const loadFile = useCallback(async (p: string) => {
    try {
      const file = await readFile(p);
      setPath(file.path);
      setContent(file.content);
      setSavedContent(file.content);
      setMode("edit");
    } catch (err) {
      console.error("read_file failed:", err);
    }
  }, []);

  const onOpen = useCallback(async () => {
    const picked = await pickFileToOpen();
    if (picked) await loadFile(picked);
  }, [loadFile]);

  const onSave = useCallback(async () => {
    let target = pathRef.current;
    if (!target) {
      target = await pickFileToSave();
      if (!target) return;
    }
    try {
      await writeFile(target, contentRef.current);
      setPath(target);
      setSavedContent(contentRef.current);
    } catch (err) {
      console.error("write_file failed:", err);
    }
  }, []);

  const onSaveAs = useCallback(async () => {
    const target = await pickFileToSave(fileName);
    if (!target) return;
    try {
      await writeFile(target, contentRef.current);
      setPath(target);
      setSavedContent(contentRef.current);
    } catch (err) {
      console.error("write_file failed:", err);
    }
  }, [fileName]);

  // Initial file (cold-start "Open With…")
  useEffect(() => {
    getInitialFile().then((p) => {
      if (p) loadFile(p);
    });
  }, [loadFile]);

  // Runtime "Open With…" event from Rust
  useEffect(() => {
    const unlisten = listen<string>("open-file", (e) => {
      if (e.payload) loadFile(e.payload);
    });
    return () => {
      unlisten.then((u) => u());
    };
  }, [loadFile]);

  // Keyboard shortcuts
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const meta = e.metaKey || e.ctrlKey;
      if (!meta) return;
      const k = e.key.toLowerCase();
      if (k === "o") {
        e.preventDefault();
        void onOpen();
      } else if (k === "s") {
        e.preventDefault();
        if (e.shiftKey) void onSaveAs();
        else void onSave();
      } else if (k === "e") {
        e.preventDefault();
        setMode("edit");
      } else if (k === "p") {
        e.preventDefault();
        setMode("preview");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onOpen, onSave, onSaveAs]);

  // Track system theme changes
  useEffect(() => {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const handler = (e: MediaQueryListEvent) => setTheme(e.matches ? "dark" : "light");
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  }, []);

  // Reflect document title
  useEffect(() => {
    const title = `${dirty ? "● " : ""}${fileName} — Markpad`;
    document.title = title;
  }, [dirty, fileName]);

  return (
    <div className="app" data-theme={theme}>
      <header className="titlebar">
        <div className="title-spacer" />
        <div className="title-name">
          {dirty && <span className="dot" aria-label="unsaved" />}
          {fileName}
        </div>
        <div className="title-actions" />
      </header>

      <div className="tabbar">
        <div className="tabs" role="tablist">
          <button
            role="tab"
            aria-selected={mode === "edit"}
            className={"tab " + (mode === "edit" ? "active" : "")}
            onClick={() => setMode("edit")}
          >
            Edit
          </button>
          <button
            role="tab"
            aria-selected={mode === "preview"}
            className={"tab " + (mode === "preview" ? "active" : "")}
            onClick={() => setMode("preview")}
          >
            Preview
          </button>
        </div>

        <div className="toolbar">
          {mode === "edit" ? (
            <>
              <Select
                label="Indent"
                value={useTabs ? "tabs" : "spaces"}
                options={[
                  { value: "spaces", label: "Spaces" },
                  { value: "tabs", label: "Tabs" },
                ]}
                onChange={(v) => setUseTabs(v === "tabs")}
              />
              <Select
                label="Width"
                value={String(tabSize)}
                options={[2, 4, 8].map((n) => ({ value: String(n), label: String(n) }))}
                onChange={(v) => setTabSize(Number(v))}
              />
              <Select
                label="Wrap"
                value={softWrap ? "soft" : "no"}
                options={[
                  { value: "soft", label: "Soft wrap" },
                  { value: "no", label: "No wrap" },
                ]}
                onChange={(v) => setSoftWrap(v === "soft")}
              />
            </>
          ) : (
            <button className="ghost" onClick={() => setMode("edit")}>
              Back to edit
            </button>
          )}
        </div>
      </div>

      <main className="pane">
        {mode === "edit" ? (
          <Editor
            value={content}
            onChange={setContent}
            tabSize={tabSize}
            useTabs={useTabs}
            softWrap={softWrap}
            theme={theme}
          />
        ) : (
          <Preview source={content} theme={theme} />
        )}
      </main>

      <footer className="statusbar">
        <span>{path ?? "Unsaved buffer"}</span>
        <span>
          {content.length.toLocaleString()} chars · {content.split(/\n/).length.toLocaleString()} lines
          {dirty ? " · Modified" : ""}
        </span>
      </footer>
    </div>
  );
}

type SelectProps = {
  label: string;
  value: string;
  options: { value: string; label: string }[];
  onChange: (v: string) => void;
};

function Select({ label, value, options, onChange }: SelectProps) {
  return (
    <label className="select">
      <span className="select-label">{label}</span>
      <select value={value} onChange={(e) => onChange(e.target.value)}>
        {options.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      </select>
    </label>
  );
}
