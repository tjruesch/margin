import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { undo, redo } from "@codemirror/commands";
import type { ReactCodeMirrorRef } from "@uiw/react-codemirror";
import { Editor } from "./Editor";
import { Home } from "./Home";
import { Preview } from "./Preview";
import { RecordingBanner, type NoteRecording } from "./RecordingBanner";
import { RestrictedBanner } from "./RestrictedBanner";
import { Settings } from "./Settings";
import { TranscriptView } from "./Transcript";
import {
  convertExternal,
  createNote,
  deleteNote,
  discardRecording,
  getInitialFile,
  hasAnthropicApiKey,
  isOwnedNote,
  listNotes,
  type NoteListItem,
  noteMeta,
  notesDir as fetchNotesDir,
  type Transcript,
  pickFileToOpen,
  pickFileToSave,
  readFile,
  readNote,
  reconcileNotes,
  setNoteTags,
  startMeetingRecording,
  stopMeetingRecording,
  transcribe,
  unwatchFile,
  watchFile,
  writeFile,
  writeNote,
} from "./file";
import {
  DEFAULT_SETTINGS,
  addRecentFile,
  loadSettings,
  removeRecentFile,
  saveAI,
  saveTheme,
  type AISettings,
  type ThemeSettings,
} from "./settingsStore";
import { applyTheme, getTheme, DEFAULT_LIGHT_THEME_ID, DEFAULT_DARK_THEME_ID } from "./themes";
import { NoteHeader } from "./NoteHeader";
import "./App.css";

type Mode = "home" | "edit" | "preview" | "transcript" | "settings";

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

/// Replace the first `# H1` line with `newTitle`, preserving any leading
/// indentation. If no H1 exists, prepend one.
function rewriteH1(content: string, newTitle: string): string {
  const lines = content.split("\n");
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const trimmed = line.trimStart();
    if (trimmed.startsWith("# ")) {
      const indent = line.slice(0, line.length - trimmed.length);
      lines[i] = `${indent}# ${newTitle}`;
      return lines.join("\n");
    }
  }
  return `# ${newTitle}\n\n${content}`;
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
  const [modifiedMs, setModifiedMs] = useState<number | null>(null);

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

  // Tags + extras live in lockstep with the active note. The editor body
  // never sees the YAML frontmatter; tag mutations write the disk via
  // set_note_tags so the in-flight buffer isn't disturbed.
  const [tags, setTags] = useState<string[]>([]);
  const [frontmatterExtras, setFrontmatterExtras] = useState<Record<string, unknown>>({});

  // Single source of truth for the home feed AND for the note-header tag
  // autocomplete. Loaded once on mount, refreshed on home navigation and
  // after note mutations; updated optimistically on tag edits so the
  // sidebar / autocomplete stay in sync without an extra disk walk.
  const [notes, setNotes] = useState<NoteListItem[]>([]);
  const [notesLoading, setNotesLoading] = useState<boolean>(true);
  const allTags = useMemo(() => {
    const set = new Set<string>();
    for (const n of notes) for (const t of n.tags) set.add(t);
    return Array.from(set).sort();
  }, [notes]);

  const contentRef = useRef(content);
  const pathRef = useRef(path);
  const savedRef = useRef(savedContent);
  const recentFilesRef = useRef<string[]>(recentFiles);
  const recordingRef = useRef<NoteRecording>(recording);
  const aiRef = useRef<AISettings>(aiSettings);
  const editorRef = useRef<ReactCodeMirrorRef>(null);
  const notesDirRef = useRef<string | null>(null);
  const tagsRef = useRef<string[]>(tags);
  const frontmatterExtrasRef = useRef<Record<string, unknown>>(frontmatterExtras);
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
  useEffect(() => {
    notesDirRef.current = notesDir;
  }, [notesDir]);
  useEffect(() => {
    tagsRef.current = tags;
  }, [tags]);
  useEffect(() => {
    frontmatterExtrasRef.current = frontmatterExtras;
  }, [frontmatterExtras]);

  /** True iff `p` lives under the owned-notes directory. Cheaper than the
   *  Tauri `isOwnedNote` round-trip; safe to call before notesDir loads
   *  (returns false in that brief window). */
  const isOwnedPath = useCallback((p: string): boolean => {
    const dir = notesDirRef.current;
    if (!dir) return false;
    const prefix = dir.endsWith("/") ? dir : dir + "/";
    return p.startsWith(prefix);
  }, []);

  /** Re-scan owned notes from disk and update the shared state. The home
   *  feed and the note-header tag autocomplete both read from this. */
  const refreshNotes = useCallback(async () => {
    try {
      const items = await listNotes();
      setNotes(items);
    } catch (err) {
      console.error("listNotes failed:", err);
    } finally {
      setNotesLoading(false);
    }
  }, []);

  // Detect whether the bundle has a transcript on disk, and whether that
  // transcript has already been reconciled (`reconciled_at` set by
  // reconcile_notes), and reset banner state accordingly.
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
    let reconciled = false;
    if (exists) {
      try {
        const f = await readFile(tp);
        const parsed: Partial<Transcript> = JSON.parse(f.content);
        reconciled = !!parsed.reconciled_at;
      } catch {
        reconciled = false;
      }
    }
    setRecording({
      kind: "none",
      hasTranscript: exists,
      transcriptPath: exists ? tp : undefined,
      reconciled,
    });
  }, []);

  const loadFile = useCallback(
    async (p: string) => {
      try {
        if (isOwnedPath(p)) {
          const note = await readNote(p);
          setPath(p);
          setContent(note.body);
          setSavedContent(note.body);
          setTags(note.tags);
          setFrontmatterExtras(note.frontmatter_extras ?? {});
        } else {
          const file = await readFile(p);
          setPath(file.path);
          setContent(file.content);
          setSavedContent(file.content);
          setTags([]);
          setFrontmatterExtras({});
        }
        setMode("edit");
        setExternalChange(null);
        setExternallyDeleted(false);
        addRecentFile(p, recentFilesRef.current)
          .then(setRecentFiles)
          .catch((err) => console.error("addRecentFile failed:", err));
        await refreshRecordingState(p);
      } catch (err) {
        console.error("loadFile failed:", err);
      }
    },
    [isOwnedPath, refreshRecordingState],
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
      await transcribe(wavPath, aiRef.current.glossary, aiRef.current.whisperModel);
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
      setRecording({ kind: "transcribing", phase: "asr", pct: 0 });
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
          aiRef.current.glossary,
        );
        setContent(md);
        // Persist immediately — reconcile is expensive (a Claude call) and
        // the autosave debounce isn't a strong enough guarantee for output
        // the user just paid for. Don't lose it to a window close.
        try {
          if (isOwnedPath(notePath)) {
            await writeNote(notePath, md, tagsRef.current, frontmatterExtrasRef.current);
          } else {
            await writeFile(notePath, md);
          }
          setSavedContent(md);
        } catch (err) {
          console.error("post-reconcile save failed:", err);
        }
        setRecording({
          kind: "none",
          hasTranscript: true,
          transcriptPath,
          reconciled: true,
        });
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
    const unPhase = listen<string>("transcribe-phase", (e) => {
      if (e.payload === "diarizing") {
        setRecording((s) =>
          s.kind === "transcribing" ? { ...s, phase: "diar", modelDl: undefined } : s,
        );
      }
    });
    const unSys = listen<string>("sysaudio-unavailable", () => {
      setSysAvailable(false);
    });
    return () => {
      unTr.then((u) => u());
      unDl.then((u) => u());
      unPhase.then((u) => u());
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
      if (isOwnedPath(target)) {
        await writeNote(
          target,
          contentRef.current,
          tagsRef.current,
          frontmatterExtrasRef.current,
        );
      } else {
        await writeFile(target, contentRef.current);
      }
      setPath(target);
      setSavedContent(contentRef.current);
      setExternalChange(null);
      setExternallyDeleted(false);
    } catch (err) {
      console.error("save failed:", err);
    }
  }, [isOwnedPath]);

  const onSaveAs = useCallback(async () => {
    const target = await pickFileToSave(fileName);
    if (!target) return;
    try {
      // Save-As lands at a user-chosen path which is virtually always
      // outside the owned-notes directory; treat it as external and write
      // the body verbatim (frontmatter and all if the user had any).
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

  // Refresh mtime for the active note whenever a save lands or path changes.
  // savedContent flips in lockstep with disk state (loadFile, autosave,
  // onSave, post-reconcile, external-change merge), so this effect catches
  // every "the file on disk just changed" moment without per-call plumbing.
  useEffect(() => {
    if (!path) {
      setModifiedMs(null);
      return;
    }
    noteMeta(path)
      .then((m) => setModifiedMs(m.modified_ms))
      .catch(() => setModifiedMs(null));
  }, [path, savedContent]);

  // Debounced autosave. Fires 800ms after the last edit, skipped when there's
  // no path (untitled buffer), nothing changed, or a disk-state conflict
  // needs the user's attention. Self-induced writes are suppressed by
  // WriteGuard in lib.rs so this doesn't echo back as an external-change.
  useEffect(() => {
    if (!path) return;
    if (content === savedContent) return;
    if (externalChange || externallyDeleted) return;
    const t = setTimeout(async () => {
      try {
        if (isOwnedPath(path)) {
          await writeNote(path, content, tagsRef.current, frontmatterExtrasRef.current);
        } else {
          await writeFile(path, content);
        }
        setSavedContent(content);
      } catch (err) {
        console.error("autosave failed:", err);
      }
    }, 800);
    return () => clearTimeout(t);
  }, [content, path, savedContent, externalChange, externallyDeleted, isOwnedPath]);

  // External-change handler: reload silently if buffer is clean, else show banner.
  useEffect(() => {
    const unlisten = listen<string>("external-change", async (e) => {
      if (!e.payload) return;
      try {
        let nextBody: string;
        let nextTags: string[] | null = null;
        let nextExtras: Record<string, unknown> | null = null;
        if (isOwnedPath(e.payload)) {
          const note = await readNote(e.payload);
          nextBody = note.body;
          nextTags = note.tags;
          nextExtras = note.frontmatter_extras ?? {};
        } else {
          const f = await readFile(e.payload);
          nextBody = f.content;
        }
        if (nextBody === savedRef.current) {
          // The body didn't change. Pick up any tag-only edits made via
          // an external editor and move on without disturbing the buffer.
          if (nextTags) setTags(nextTags);
          if (nextExtras) setFrontmatterExtras(nextExtras);
          return;
        }
        if (contentRef.current === savedRef.current) {
          setContent(nextBody);
          setSavedContent(nextBody);
          if (nextTags) setTags(nextTags);
          if (nextExtras) setFrontmatterExtras(nextExtras);
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
  }, [isOwnedPath]);

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
      if (isOwnedPath(externalChange.path)) {
        const note = await readNote(externalChange.path);
        setContent(note.body);
        setSavedContent(note.body);
        setTags(note.tags);
        setFrontmatterExtras(note.frontmatter_extras ?? {});
      } else {
        const f = await readFile(externalChange.path);
        setContent(f.content);
        setSavedContent(f.content);
      }
      setExternalChange(null);
    } catch (err) {
      console.error("reload failed:", err);
    }
  }, [externalChange, isOwnedPath]);

  // Track system theme changes
  useEffect(() => {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const handler = (e: MediaQueryListEvent) =>
      setSystemAppearance(e.matches ? "dark" : "light");
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  }, []);

  // Manual window-drag handler. Tauri 2's auto-injected `data-tauri-drag-
  // region` listener doesn't fire reliably on macOS windows that combine
  // `transparent: true` + `titleBarStyle: Overlay` + `hiddenTitle: true`
  // (issue tauri-apps/tauri#10662 & friends). We walk ancestors on
  // mousedown, look for the attribute, and call startDragging() ourselves.
  // Double-click in the same region toggles maximize, matching the macOS
  // title-bar convention.
  useEffect(() => {
    const win = getCurrentWindow();
    const onMouseDown = (e: MouseEvent) => {
      if (e.button !== 0) return;
      let el = e.target as HTMLElement | null;
      while (el) {
        const flag = el.dataset.tauriDragRegion;
        if (flag === "false") return;
        if (flag !== undefined) break;
        el = el.parentElement;
      }
      if (!el) return;
      e.preventDefault();
      if (e.detail >= 2) {
        void win.toggleMaximize();
      } else {
        void win.startDragging();
      }
    };
    document.addEventListener("mousedown", onMouseDown);
    return () => document.removeEventListener("mousedown", onMouseDown);
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
    void refreshNotes();
  }, []);

  // Re-scan notes whenever the user enters home so the feed reflects any
  // edits made in the editor since the last visit.
  useEffect(() => {
    if (mode === "home") void refreshNotes();
    // refreshNotes is stable (defined below with empty deps)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode]);

  const onTagsChange = useCallback(
    async (next: string[]) => {
      const target = pathRef.current;
      if (!target || !isOwnedPath(target)) return;
      setTags(next);
      // Optimistically reflect the change in the shared `notes` state so
      // the home feed and the note-header autocomplete re-derive `allTags`
      // immediately — no extra disk walk required.
      setNotes((prev) =>
        prev.map((n) => (n.note_path === target ? { ...n, tags: next } : n)),
      );
      try {
        await setNoteTags(target, next);
      } catch (err) {
        console.error("setNoteTags failed:", err);
      }
    },
    [isOwnedPath],
  );

  const onDeleteNote = useCallback(
    async (explicitPath?: string) => {
      const target = explicitPath ?? pathRef.current;
      if (!target || !isOwnedPath(target)) return;
      const ok = await ask("This note and any associated recording will be permanently deleted.", {
        title: "Delete note?",
        kind: "warning",
        okLabel: "Delete",
        cancelLabel: "Cancel",
      });
      if (!ok) return;

      try {
        await deleteNote(target);
      } catch (err) {
        console.error("deleteNote failed:", err);
        alert("Could not delete the note. See console for details.");
        return;
      }

      try {
        const nextRecent = await removeRecentFile(target, recentFilesRef.current);
        setRecentFiles(nextRecent);
      } catch (err) {
        console.warn("removeRecentFile failed:", err);
      }

      // Tear down editor state only if we deleted the currently-open note.
      // Deleting an unrelated row from Home leaves the editor untouched.
      const wasOpen = target === pathRef.current;
      if (wasOpen) {
        try {
          await unwatchFile();
        } catch {
          /* ignore */
        }
        setPath(null);
        setContent(WELCOME);
        setSavedContent(WELCOME);
        setTags([]);
        setFrontmatterExtras({});
        setRecording({ kind: "none", hasTranscript: false });
        setExternalChange(null);
        setExternallyDeleted(false);
      }

      await refreshNotes();
      if (wasOpen) setMode("home");
    },
    [isOwnedPath, refreshNotes],
  );

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

  const showTabbar = mode === "edit" || mode === "preview" || mode === "transcript";

  // Path to the active note's transcript.json, if one exists. Pulled from
  // the recording state machine — kind: "none" with hasTranscript, or any
  // post-stop kind that already carries a transcriptPath.
  const transcriptPath: string | undefined =
    recording.kind === "none" && recording.hasTranscript
      ? recording.transcriptPath
      : recording.kind === "ready"
        ? recording.transcriptPath
        : recording.kind === "error" && recording.transcriptPath
          ? recording.transcriptPath
          : undefined;
  const hasTranscript = !!transcriptPath;

  // If the user was looking at the transcript and it disappeared (recording
  // discarded, or note swapped to one without one), bounce back to edit.
  useEffect(() => {
    if (mode === "transcript" && !hasTranscript) setMode("edit");
  }, [mode, hasTranscript]);
  const showRecordingBanner = mode === "edit" && isOwned;
  const showRestrictedBanner = mode === "edit" && !isOwned && path !== null;

  const noteTitle = useMemo(() => deriveTitle(content, fileName), [content, fileName]);

  // Has the active note's transcript already been reconciled at least
  // once? Read from transcript.json's `reconciled_at` field via the
  // recording state machine. Used to suppress the post-recording
  // Generate-notes CTA after the user has run it.
  const notesGenerated = recording.kind === "none" && !!recording.reconciled;

  const onTitleChange = useCallback((next: string) => {
    setContent((cur) => rewriteH1(cur, next));
  }, []);

  const onEditorPrefsChange = useCallback(
    (next: { tabSize: number; useTabs: boolean; softWrap: boolean }) => {
      setTabSize(next.tabSize);
      setUseTabs(next.useTabs);
      setSoftWrap(next.softWrap);
    },
    [],
  );

  const canRecord = isOwned && recording.kind === "none";

  return (
    <div className="app" data-theme={theme}>
      {showTabbar && (
        <NoteHeader
          title={noteTitle}
          onTitleChange={onTitleChange}
          mode={
            mode === "preview" ? "preview" : mode === "transcript" ? "transcript" : "edit"
          }
          onModeChange={(m) => tryNavigate(m)}
          hasTranscript={hasTranscript}
          recording={recording.kind === "recording"}
          canRecord={canRecord}
          onStartRecord={() => void startRecordingForCurrent()}
          onStopRecord={() => void onStopRecording()}
          modifiedMs={modifiedMs}
          tags={tags}
          allTags={allTags}
          tagsEditable={isOwned}
          onTagsChange={(next) => void onTagsChange(next)}
          onBack={() => tryNavigate("home")}
          onDelete={
            isOwned && recording.kind === "none"
              ? () => void onDeleteNote()
              : undefined
          }
        />
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
          notesGenerated={notesGenerated}
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
        {mode === "transcript" && transcriptPath && (
          <TranscriptView path={transcriptPath} />
        )}
        {mode === "settings" && (
          <Settings
            theme={themeSettings}
            ai={aiSettings}
            editor={{ tabSize, useTabs, softWrap }}
            onThemeChange={onThemeChange}
            onAIChange={onAIChange}
            onEditorChange={onEditorPrefsChange}
            onBack={() => tryNavigate("home")}
          />
        )}
        {mode === "home" && (
          <Home
            recentFiles={recentFiles}
            notes={notes}
            notesLoading={notesLoading}
            allTags={allTags}
            onOpen={(p) => void loadFile(p)}
            onNewNote={() => void onNewNote()}
            onNewMeeting={() => void onNewMeeting()}
            onOpenSettings={() => tryNavigate("settings")}
            onDeleteRow={(p) => void onDeleteNote(p)}
          />
        )}
      </main>

      {mode !== "home" && mode !== "settings" && (
        <footer className="statusbar">
          <span>
            {content.length.toLocaleString()} chars · {content.split(/\n/).length.toLocaleString()} lines
            {dirty ? " · Modified" : ""}
          </span>
        </footer>
      )}
    </div>
  );
}

