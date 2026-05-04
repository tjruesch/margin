import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { undo, redo } from "@codemirror/commands";
import type { ReactCodeMirrorRef } from "@uiw/react-codemirror";
import { Editor } from "./Editor";
import { Home } from "./Home";
import { Preview } from "./Preview";
import { RecordingBanner, type NoteRecording } from "./RecordingBanner";
import { RestrictedBanner } from "./RestrictedBanner";
import { Settings } from "./Settings";
import {
  convertExternal,
  createNote,
  discardRecording,
  getInitialFile,
  hasAnthropicApiKey,
  isOwnedNote,
  notesDir as fetchNotesDir,
  pickFileToOpen,
  pickFileToSave,
  readFile,
  reconcileNotes,
  startMeetingRecording,
  stopMeetingRecording,
  transcribe,
  unwatchFile,
  watchFile,
  writeFile,
} from "./file";
import {
  DEFAULT_SETTINGS,
  addRecentFile,
  loadSettings,
  saveAI,
  saveTheme,
  type AISettings,
  type ThemeSettings,
} from "./settingsStore";
import { applyTheme, getTheme, DEFAULT_LIGHT_THEME_ID, DEFAULT_DARK_THEME_ID } from "./themes";
import "./App.css";

type Mode = "home" | "edit" | "preview" | "settings";

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

function transcriptPathFor(notePath: string): string {
  return notePath.replace(/note\.md$/, "transcript.json");
}

function deriveTitle(content: string, fallback: string): string {
  for (const line of content.split("\n")) {
    const trimmed = line.trimStart();
    if (trimmed.startsWith("# ")) {
      const t = trimmed.slice(2).trim();
      if (t.length > 0) return t;
    }
  }
  return fallback;
}

export default function App() {
  const [mode, setMode] = useState<Mode>("home");
  const [recentFiles, setRecentFiles] = useState<string[]>([]);
  const [content, setContent] = useState<string>(WELCOME);
  const [path, setPath] = useState<string | null>(null);
  const [savedContent, setSavedContent] = useState<string>(WELCOME);
  const [tabSize, setTabSize] = useState<number>(2);
  const [useTabs, setUseTabs] = useState<boolean>(false);
  const [softWrap, setSoftWrap] = useState<boolean>(true);
  const [themeSettings, setThemeSettings] = useState<ThemeSettings>(DEFAULT_SETTINGS.theme);
  const [aiSettings, setAISettings] = useState<AISettings>(DEFAULT_SETTINGS.ai);
  const [systemAppearance, setSystemAppearance] = useState<"light" | "dark">(systemTheme);
  const [notesDir, setNotesDir] = useState<string | null>(null);
  const [hasKey, setHasKey] = useState<boolean>(true);
  const [sysAvailable, setSysAvailable] = useState<boolean>(true);
  const [recording, setRecording] = useState<NoteRecording>({
    kind: "none",
    hasTranscript: false,
  });

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

  useEffect(() => {
    applyTheme(activeTheme);
  }, [activeTheme]);

  const [externalChange, setExternalChange] = useState<{ path: string } | null>(null);
  const [externallyDeleted, setExternallyDeleted] = useState<boolean>(false);

  const isOwned = useMemo(() => {
    if (!path || !notesDir) return false;
    return path.startsWith(notesDir.endsWith("/") ? notesDir : notesDir + "/");
  }, [path, notesDir]);

  const recordingExclusive =
    recording.kind === "recording" ||
    recording.kind === "transcribing" ||
    recording.kind === "reconciling";

  const tryNavigate = useCallback(
    (next: Mode) => {
      if (recordingExclusive) return;
      setMode(next);
    },
    [recordingExclusive],
  );

  const dirty = content !== savedContent;
  const fileName = path ? path.split("/").pop() ?? "Untitled.md" : "Untitled.md";

  const contentRef = useRef(content);
  const pathRef = useRef(path);
  const savedRef = useRef(savedContent);
  const recentFilesRef = useRef<string[]>(recentFiles);
  const recordingRef = useRef<NoteRecording>(recording);
  const aiRef = useRef<AISettings>(aiSettings);
  const editorRef = useRef<ReactCodeMirrorRef>(null);
  useEffect(() => {
    recentFilesRef.current = recentFiles;
  }, [recentFiles]);
  useEffect(() => {
    contentRef.current = content;
  }, [content]);
  useEffect(() => {
    pathRef.current = path;
  }, [path]);
  useEffect(() => {
    savedRef.current = savedContent;
  }, [savedContent]);
  useEffect(() => {
    recordingRef.current = recording;
  }, [recording]);
  useEffect(() => {
    aiRef.current = aiSettings;
  }, [aiSettings]);

  // Detect whether the bundle has a transcript on disk and reset banner state.
  const refreshRecordingState = useCallback(async (notePath: string | null) => {
    if (!notePath || !notePath.endsWith("/note.md")) {
      setRecording({ kind: "none", hasTranscript: false });
      return;
    }
    const tp = transcriptPathFor(notePath);
    let exists = false;
    try {
      exists = await invoke<boolean>("file_exists", { path: tp });
    } catch {
      exists = false;
    }
    setRecording({
      kind: "none",
      hasTranscript: exists,
      transcriptPath: exists ? tp : undefined,
    });
  }, []);

  const loadFile = useCallback(
    async (p: string) => {
      try {
        const file = await readFile(p);
        setPath(file.path);
        setContent(file.content);
        setSavedContent(file.content);
        setMode("edit");
        setExternalChange(null);
        setExternallyDeleted(false);
        addRecentFile(file.path, recentFilesRef.current)
          .then(setRecentFiles)
          .catch((err) => console.error("addRecentFile failed:", err));
        await refreshRecordingState(file.path);
      } catch (err) {
        console.error("read_file failed:", err);
      }
    },
    [refreshRecordingState],
  );

  // ----- recording state machine ----------------------------------------

  const startRecordingForCurrent = useCallback(async () => {
    const current = pathRef.current;
    if (!current) return;
    const owned = await isOwnedNote(current);
    if (!owned) return;
    if (recordingRef.current.kind !== "none") return;
    try {
      await startMeetingRecording(current, aiRef.current.recordSystemAudio);
      setSysAvailable(true);
      setRecording({ kind: "recording", startedAt: Date.now() });
    } catch (err) {
      setRecording({
        kind: "error",
        message: typeof err === "string" ? err : "Failed to start recording.",
      });
    }
  }, []);

  const runTranscribe = useCallback(async (wavPath: string) => {
    try {
      await transcribe(wavPath);
      const notePath = pathRef.current;
      const tp = notePath ? transcriptPathFor(notePath) : wavPath.replace(/\/audio\.wav$/, "/transcript.json");
      setRecording({ kind: "ready", transcriptPath: tp });
    } catch (err) {
      setRecording({
        kind: "error",
        message: typeof err === "string" ? err : "Transcription failed.",
      });
    }
  }, []);

  const onStopRecording = useCallback(async () => {
    if (recordingRef.current.kind !== "recording") return;
    try {
      const wavPath = await stopMeetingRecording();
      setRecording({ kind: "transcribing", pct: 0 });
      void runTranscribe(wavPath);
    } catch (err) {
      setRecording({
        kind: "error",
        message: typeof err === "string" ? err : "Failed to stop recording.",
      });
    }
  }, [runTranscribe]);

  const onDiscardRecording = useCallback(async () => {
    const current = recordingRef.current;
    const notePath = pathRef.current;
    if (current.kind === "recording") {
      try {
        await stopMeetingRecording();
      } catch {
        /* ignore — we're discarding anyway */
      }
    }
    if (notePath) {
      try {
        await discardRecording(notePath);
      } catch (err) {
        console.warn("discard_recording failed:", err);
      }
    }
    setRecording({ kind: "none", hasTranscript: false });
  }, []);

  const runReconcile = useCallback(
    async (transcriptPath: string) => {
      const notePath = pathRef.current;
      if (!notePath) return;
      const fallback = (notePath.split("/").pop() ?? "Untitled note").replace(
        /\.md$/,
        "",
      );
      const title = deriveTitle(contentRef.current, fallback);
      setRecording({ kind: "reconciling" });
      try {
        const md = await reconcileNotes(
          contentRef.current,
          transcriptPath,
          title,
          aiRef.current.summaryModel,
        );
        setContent(md);
        setRecording({ kind: "none", hasTranscript: true, transcriptPath });
      } catch (err) {
        setRecording({
          kind: "error",
          message: typeof err === "string" ? err : "Reconciliation failed.",
          transcriptPath,
        });
      }
    },
    [],
  );

  const onGenerate = useCallback(() => {
    const r = recordingRef.current;
    let tp: string | undefined;
    if (r.kind === "ready") tp = r.transcriptPath;
    else if (r.kind === "none" && r.hasTranscript) tp = r.transcriptPath;
    else if (r.kind === "error" && r.transcriptPath) tp = r.transcriptPath;
    if (!tp) return;
    void runReconcile(tp);
  }, [runReconcile]);

  const onDismissError = useCallback(() => {
    void refreshRecordingState(pathRef.current);
  }, [refreshRecordingState]);

  // transcribe-progress + model-download-progress
  useEffect(() => {
    const unTr = listen<number>("transcribe-progress", (e) => {
      const pct = typeof e.payload === "number" ? e.payload : 0;
      setRecording((s) => (s.kind === "transcribing" ? { ...s, pct } : s));
    });
    const unDl = listen<{ downloaded: number; total: number }>(
      "model-download-progress",
      (e) => {
        if (!e.payload) return;
        setRecording((s) =>
          s.kind === "transcribing"
            ? { ...s, modelDl: { downloaded: e.payload.downloaded, total: e.payload.total } }
            : s,
        );
      },
    );
    const unSys = listen<string>("sysaudio-unavailable", () => {
      setSysAvailable(false);
    });
    return () => {
      unTr.then((u) => u());
      unDl.then((u) => u());
      unSys.then((u) => u());
    };
  }, []);

  // ----- new note / new meeting -----------------------------------------

  const onNewNote = useCallback(async () => {
    try {
      const ref = await createNote();
      await loadFile(ref.note_path);
    } catch (err) {
      console.error("create_note failed:", err);
    }
  }, [loadFile]);

  const onNewMeeting = useCallback(async () => {
    try {
      const ref = await createNote();
      await loadFile(ref.note_path);
      // loadFile sets pathRef synchronously via setState, but state updates are
      // queued; explicitly start once the next tick rolls in.
      await startMeetingRecording(ref.note_path, aiRef.current.recordSystemAudio);
      setSysAvailable(true);
      setRecording({ kind: "recording", startedAt: Date.now() });
    } catch (err) {
      console.error("new meeting failed:", err);
      setRecording({
        kind: "error",
        message: typeof err === "string" ? err : "Failed to start recording.",
      });
    }
  }, [loadFile]);

  const onConvertExternal = useCallback(async () => {
    const src = pathRef.current;
    if (!src) return;
    try {
      const ref = await convertExternal(src);
      await loadFile(ref.note_path);
    } catch (err) {
      console.error("convert_external failed:", err);
    }
  }, [loadFile]);

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
        case "file_new_note":
          if (recordingRef.current.kind === "none" || recordingRef.current.kind === "ready") {
            void onNewNote();
          }
          break;
        case "file_record":
          if (recordingRef.current.kind !== "none") break;
          if (mode === "edit" && isOwned && pathRef.current) {
            void startRecordingForCurrent();
          } else {
            void onNewMeeting();
          }
          break;
        case "file_home":
          tryNavigate("home");
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
  }, [
    isOwned,
    mode,
    onNewMeeting,
    onNewNote,
    onOpen,
    onSave,
    onSaveAs,
    startRecordingForCurrent,
    tryNavigate,
  ]);

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

  // Track system theme changes
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
        setRecentFiles(s.recentFiles);
      })
      .catch((err) => console.error("loadSettings failed:", err));
    fetchNotesDir()
      .then(setNotesDir)
      .catch((err) => console.error("notes_dir failed:", err));
    hasAnthropicApiKey()
      .then(setHasKey)
      .catch(() => setHasKey(false));
  }, []);

  // Refresh API-key status whenever settings change (or banner re-enters idle).
  useEffect(() => {
    if (mode === "settings" || recording.kind === "none" || recording.kind === "ready") {
      hasAnthropicApiKey()
        .then(setHasKey)
        .catch(() => setHasKey(false));
    }
  }, [mode, recording.kind]);

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

  const showTabbar = mode === "edit" || mode === "preview";
  const showRecordingBanner = mode === "edit" && isOwned;
  const showRestrictedBanner = mode === "edit" && !isOwned && path !== null;

  return (
    <div className="app" data-theme={theme}>
      {!showTabbar && <div className="drag-bar" data-tauri-drag-region />}
      {showTabbar && (
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
      )}

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

      {showRecordingBanner && (
        <RecordingBanner
          state={recording}
          recordingSysAudio={aiSettings.recordSystemAudio}
          sysAvailable={sysAvailable}
          summaryModel={aiSettings.summaryModel}
          hasKey={hasKey}
          onStart={() => void startRecordingForCurrent()}
          onStop={() => void onStopRecording()}
          onDiscard={() => void onDiscardRecording()}
          onGenerate={onGenerate}
          onDismissError={onDismissError}
        />
      )}
      {showRestrictedBanner && <RestrictedBanner onConvert={() => void onConvertExternal()} />}

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
        {mode === "home" && (
          <Home
            recentFiles={recentFiles}
            onOpen={(p) => void loadFile(p)}
            onNewNote={() => void onNewNote()}
            onNewMeeting={() => void onNewMeeting()}
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
