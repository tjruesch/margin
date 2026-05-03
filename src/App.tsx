import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { undo, redo } from "@codemirror/commands";
import type { ReactCodeMirrorRef } from "@uiw/react-codemirror";
import { Editor } from "./Editor";
import { Meeting } from "./Meeting";
import { Preview } from "./Preview";
import { Settings } from "./Settings";
import {
  getInitialFile,
  pickFileToOpen,
  pickFileToSave,
  readFile,
  unwatchFile,
  watchFile,
  writeFile,
} from "./file";
import {
  DEFAULT_SETTINGS,
  loadSettings,
  saveAI,
  saveTheme,
  type AISettings,
  type ThemeSettings,
} from "./settingsStore";
import { applyTheme, getTheme, DEFAULT_LIGHT_THEME_ID, DEFAULT_DARK_THEME_ID } from "./themes";
import "./App.css";

type Mode = "edit" | "preview" | "settings" | "meeting";

function systemTheme(): "light" | "dark" {
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

const WELCOME = `# Welcome to Margin

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
  const [themeSettings, setThemeSettings] = useState<ThemeSettings>(DEFAULT_SETTINGS.theme);
  const [aiSettings, setAISettings] = useState<AISettings>(DEFAULT_SETTINGS.ai);
  const [systemAppearance, setSystemAppearance] = useState<"light" | "dark">(systemTheme);

  // Resolve the active theme id from settings + system appearance.
  const activeThemeId = themeSettings.syncWithOS
    ? systemAppearance === "dark"
      ? themeSettings.darkTheme
      : themeSettings.lightTheme
    : themeSettings.fixedTheme;
  const activeTheme =
    getTheme(activeThemeId) ??
    getTheme(systemAppearance === "dark" ? DEFAULT_DARK_THEME_ID : DEFAULT_LIGHT_THEME_ID)!;
  const theme: "light" | "dark" = activeTheme.appearance;

  // Apply CSS variables whenever the resolved theme changes.
  useEffect(() => {
    applyTheme(activeTheme);
  }, [activeTheme]);

  const [externalChange, setExternalChange] = useState<{ path: string } | null>(null);
  const [externallyDeleted, setExternallyDeleted] = useState<boolean>(false);
  const [meetingExclusive, setMeetingExclusive] = useState<boolean>(false);

  const tryNavigate = useCallback(
    (next: Mode) => {
      // Lock mode switching while a meeting is in an in-progress state
      // (recording / transcribing / summarizing). Idle and error states
      // don't lock.
      if (meetingExclusive && mode === "meeting") return;
      setMode(next);
    },
    [meetingExclusive, mode],
  );

  const dirty = content !== savedContent;
  const fileName = path ? path.split("/").pop() ?? "Untitled.md" : "Untitled.md";

  const contentRef = useRef(content);
  const pathRef = useRef(path);
  const savedRef = useRef(savedContent);
  const editorRef = useRef<ReactCodeMirrorRef>(null);
  useEffect(() => {
    contentRef.current = content;
  }, [content]);
  useEffect(() => {
    pathRef.current = path;
  }, [path]);
  useEffect(() => {
    savedRef.current = savedContent;
  }, [savedContent]);

  const loadFile = useCallback(async (p: string) => {
    try {
      const file = await readFile(p);
      setPath(file.path);
      setContent(file.content);
      setSavedContent(file.content);
      setMode("edit");
      setExternalChange(null);
      setExternallyDeleted(false);
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
      setExternalChange(null);
      setExternallyDeleted(false);
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
      setExternalChange(null);
      setExternallyDeleted(false);
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

  // Native menu events from Rust
  useEffect(() => {
    const unlisten = listen<string>("menu", (e) => {
      switch (e.payload) {
        case "file_open":
          void onOpen();
          break;
        case "file_save":
          void onSave();
          break;
        case "file_save_as":
          void onSaveAs();
          break;
        case "view_edit":
          tryNavigate("edit");
          break;
        case "view_preview":
          tryNavigate("preview");
          break;
        case "app_settings":
          tryNavigate("settings");
          break;
        case "file_new_meeting":
          tryNavigate("meeting");
          break;
        case "edit_undo": {
          const v = editorRef.current?.view;
          if (v) {
            undo(v);
            v.focus();
          }
          break;
        }
        case "edit_redo": {
          const v = editorRef.current?.view;
          if (v) {
            redo(v);
            v.focus();
          }
          break;
        }
      }
    });
    return () => {
      unlisten.then((u) => u());
    };
  }, [onOpen, onSave, onSaveAs, tryNavigate]);

  // Keep View → Edit/Preview check marks in sync with React state.
  useEffect(() => {
    void invoke("set_mode_check", { mode });
  }, [mode]);

  // (Re-)arm the disk watcher whenever the active path changes.
  useEffect(() => {
    if (path) void watchFile(path);
    else void unwatchFile();
  }, [path]);

  // External-change handler: reload silently if buffer is clean, else show banner.
  useEffect(() => {
    const unlisten = listen<string>("external-change", async (e) => {
      if (!e.payload) return;
      try {
        const f = await readFile(e.payload);
        if (f.content === savedRef.current) return; // spurious (mtime-only)
        if (contentRef.current === savedRef.current) {
          setContent(f.content);
          setSavedContent(f.content);
          setExternalChange(null);
        } else {
          setExternalChange({ path: e.payload });
        }
      } catch {
        /* file may have just been removed; let external-delete drive UI */
      }
    });
    return () => {
      unlisten.then((u) => u());
    };
  }, []);

  // External-delete handler.
  useEffect(() => {
    const unlisten = listen<string>("external-delete", () => setExternallyDeleted(true));
    return () => {
      unlisten.then((u) => u());
    };
  }, []);

  const onReloadFromDisk = useCallback(async () => {
    if (!externalChange) return;
    try {
      const f = await readFile(externalChange.path);
      setContent(f.content);
      setSavedContent(f.content);
      setExternalChange(null);
    } catch (err) {
      console.error("reload failed:", err);
    }
  }, [externalChange]);

  // Track system theme changes (always — used when preference is "system")
  useEffect(() => {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const handler = (e: MediaQueryListEvent) =>
      setSystemAppearance(e.matches ? "dark" : "light");
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  }, []);

  // Hydrate persisted settings on mount
  useEffect(() => {
    loadSettings()
      .then((s) => {
        setThemeSettings(s.theme);
        setAISettings(s.ai);
      })
      .catch((err) => console.error("loadSettings failed:", err));
  }, []);

  const onThemeChange = useCallback((next: ThemeSettings) => {
    setThemeSettings(next);
    void saveTheme(next).catch((err) => console.error("saveTheme failed:", err));
  }, []);

  const onAIChange = useCallback((next: AISettings) => {
    setAISettings(next);
    void saveAI(next).catch((err) => console.error("saveAI failed:", err));
  }, []);

  // Reflect document title
  useEffect(() => {
    const title = `${dirty ? "● " : ""}${fileName} — Margin`;
    document.title = title;
  }, [dirty, fileName]);

  return (
    <div className="app" data-theme={theme}>
      <div className="tabbar" data-tauri-drag-region>
        <div className="tabs" role="tablist">
          <button
            role="tab"
            aria-selected={mode === "edit"}
            className={"tab " + (mode === "edit" ? "active" : "")}
            onClick={() => tryNavigate("edit")}
          >
            Edit
          </button>
          <button
            role="tab"
            aria-selected={mode === "preview"}
            className={"tab " + (mode === "preview" ? "active" : "")}
            onClick={() => tryNavigate("preview")}
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
            <button className="ghost" onClick={() => tryNavigate("edit")}>
              Back to edit
            </button>
          )}
        </div>
      </div>

      {externalChange && (
        <div className="banner banner-warn" role="alert">
          <span className="banner-msg">This file was modified on disk.</span>
          <div className="banner-actions">
            <button className="ghost" onClick={() => void onReloadFromDisk()}>
              Reload
            </button>
            <button className="ghost" onClick={() => setExternalChange(null)}>
              Keep mine
            </button>
          </div>
        </div>
      )}

      {externallyDeleted && (
        <div className="banner banner-warn" role="alert">
          <span className="banner-msg">This file was deleted on disk.</span>
          <div className="banner-actions">
            <button className="ghost" onClick={() => void onSave()}>
              Save to recreate
            </button>
            <button className="ghost" onClick={() => setExternallyDeleted(false)}>
              Dismiss
            </button>
          </div>
        </div>
      )}

      <main className="pane">
        {mode === "edit" && (
          <Editor
            ref={editorRef}
            value={content}
            onChange={setContent}
            tabSize={tabSize}
            useTabs={useTabs}
            softWrap={softWrap}
          />
        )}
        {mode === "preview" && <Preview source={content} theme={theme} />}
        {mode === "settings" && (
          <Settings
            theme={themeSettings}
            ai={aiSettings}
            onThemeChange={onThemeChange}
            onAIChange={onAIChange}
          />
        )}
        {mode === "meeting" && (
          <Meeting
            ai={aiSettings}
            onMdReady={(p) => void loadFile(p)}
            onExclusiveChange={setMeetingExclusive}
          />
        )}
      </main>

      <footer className="statusbar">
        <span>{path ?? "Not yet saved"}</span>
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
