import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { undo, redo } from "@codemirror/commands";
import type { ReactCodeMirrorRef } from "@uiw/react-codemirror";
import { Editor } from "./Editor";
import { dispatchDiff } from "./editor/applyDiff";
import { AssigneePopover } from "./editor/assigneePopover";
import { DueDatePopover } from "./editor/dueDatePopover";
import {
  loadNotifications,
  makeNotificationId,
  markAllRead,
  pushNotification,
  saveNotifications,
  type NotificationRecord,
} from "./notifications";
import { Home } from "./Home";
import { Preview } from "./Preview";
import { RecordingBanner, type NoteRecording } from "./RecordingBanner";
import { RestrictedBanner } from "./RestrictedBanner";
import { Settings } from "./Settings";
import { TranscriptView } from "./Transcript";
import { AttendeePicker } from "./AttendeePicker";
import {
  convertExternal,
  createNote,
  deleteNote,
  discardRecording,
  duplicateNote,
  ensureInboxNote,
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
  setArchived as setArchivedFile,
  setFavorite as setFavoriteFile,
  setActionAssignee,
  setActionDone,
  setMeetingAttendees,
  getMeetingAttendees,
  setNoteTags,
  shareNote,
  listActions,
  listTeamMembers,
  type ActionListItem,
  type TeamMember,
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

/// A background op tied to a specific source note. Decoupled from the
/// quiescent `recording` state so the user can navigate away from a
/// meeting while transcription or reconcile finishes. At most one op
/// is in flight at a time (enforced by the entry-point callbacks).
type InFlightOp =
  | {
      kind: "recording";
      notePath: string;
      noteTitle: string;
      startedAt: number;
    }
  | {
      kind: "transcribing";
      notePath: string;
      noteTitle: string;
      phase: "asr" | "diar";
      pct: number;
      modelDl?: { downloaded: number; total: number };
    }
  | {
      kind: "reconciling";
      notePath: string;
      noteTitle: string;
    };

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
  const [fontSize, setFontSize] = useState<number>(14);

  // Expose the editor font size as a CSS custom property so preview /
  // transcript bodies can derive their typography from it via calc().
  useEffect(() => {
    document.documentElement.style.setProperty("--editor-font-size", `${fontSize}px`);
  }, [fontSize]);
  const [themeSettings, setThemeSettings] = useState<ThemeSettings>(DEFAULT_SETTINGS.theme);
  const [aiSettings, setAISettings] = useState<AISettings>(DEFAULT_SETTINGS.ai);
  const [systemAppearance, setSystemAppearance] = useState<"light" | "dark">(systemTheme);
  const [notesDir, setNotesDir] = useState<string | null>(null);
  const [hasKey, setHasKey] = useState<boolean>(true);
  const [sysAvailable, setSysAvailable] = useState<boolean>(true);
  // Attendee picker state lives here so the imperative `askForAttendees`
  // pattern below can hand the modal a Promise resolver. Open ⇔ non-null.
  const [pickerState, setPickerState] = useState<{
    notePath: string;
    resolve: (ids: string[] | null) => void;
  } | null>(null);
  // Attendees attached to the active note, surfaced as chips on the
  // header. Empty array when the note has no saved attendees yet.
  const [meetingAttendees, setMeetingAttendeesState] = useState<TeamMember[]>(
    [],
  );
  const [recording, setRecording] = useState<NoteRecording>({
    kind: "none",
    hasTranscript: false,
  });
  // The single in-flight background op (if any). Split from `recording`
  // so the user can navigate between notes while transcription /
  // reconciliation runs without erasing or polluting either's state.
  // `recording` continues to track the *quiescent* state of the
  // currently-displayed note (none / ready / error / actively recording);
  // `inFlight` carries the source-note path of any background op, so
  // banner rendering and completion handlers know which note the result
  // belongs to.
  const [inFlight, setInFlight] = useState<InFlightOp | null>(null);
  const inFlightRef = useRef<InFlightOp | null>(null);
  useEffect(() => {
    inFlightRef.current = inFlight;
  }, [inFlight]);
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

  // Load the attendee list whenever the active note path changes. The
  // backend returns empty for non-meeting notes, which is fine — the
  // header chip is hidden when the array is empty.
  useEffect(() => {
    if (!path || !isOwned) {
      setMeetingAttendeesState([]);
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const list = await getMeetingAttendees(path);
        if (!cancelled) setMeetingAttendeesState(list);
      } catch (err) {
        console.error("getMeetingAttendees failed:", err);
        if (!cancelled) setMeetingAttendeesState([]);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [path, isOwned]);

  // Active audio recording is the only state that blocks navigation —
  // a live mic is foreground by nature. Transcription and reconcile
  // run as true background ops via the inFlight slot, so navigating
  // away from the source note while they run is safe.
  const tryNavigate = useCallback((next: Mode) => {
    if (inFlightRef.current?.kind === "recording") return;
    setMode(next);
  }, []);

  const dirty = content !== savedContent;
  const fileName = path ? path.split("/").pop() ?? "Untitled.md" : "Untitled.md";

  // Tags + extras live in lockstep with the active note. The editor body
  // never sees the YAML frontmatter; tag mutations write the disk via
  // set_note_tags so the in-flight buffer isn't disturbed.
  const [tags, setTags] = useState<string[]>([]);
  const [archived, setArchived] = useState<boolean>(false);
  const [favorite, setFavorite] = useState<boolean>(false);
  const [frontmatterExtras, setFrontmatterExtras] = useState<Record<string, unknown>>({});
  const [notesScope, setNotesScope] = useState<"active" | "archived" | "favorites">(
    "active",
  );

  // Single source of truth for the home feed AND for the note-header tag
  // autocomplete. Loaded once on mount, refreshed on home navigation and
  // after note mutations; updated optimistically on tag edits so the
  // sidebar / autocomplete stay in sync without an extra disk walk.
  const [notes, setNotes] = useState<NoteListItem[]>([]);
  const [notesLoading, setNotesLoading] = useState<boolean>(true);
  const [actions, setActions] = useState<ActionListItem[]>([]);
  // Team members for the assignee-chip dropdown on action rows (#51).
  // Loaded once at mount and refreshed on `margin:nav` events so adds /
  // edits / deletes done on the Team page are reflected next time the
  // user is back on Home or the Action items page.
  const [members, setMembers] = useState<TeamMember[]>([]);
  // In-app notifications surfaced by the title-bar bell (#37). Persisted
  // via tauri-plugin-store; survives app restarts. Capped at 50.
  const [notifications, setNotifications] = useState<NotificationRecord[]>(
    [],
  );
  // Load persisted notifications once on mount.
  useEffect(() => {
    let cancelled = false;
    void loadNotifications().then((list) => {
      if (!cancelled) setNotifications(list);
    });
    return () => {
      cancelled = true;
    };
  }, []);
  const pushNotificationAndPersist = useCallback(
    (rec: Omit<NotificationRecord, "id" | "created_ms">) => {
      const full: NotificationRecord = {
        ...rec,
        id: makeNotificationId(),
        created_ms: Date.now(),
      };
      setNotifications((curr) => {
        const next = pushNotification(curr, full);
        void saveNotifications(next);
        return next;
      });
    },
    [],
  );
  const markNotificationsRead = useCallback(() => {
    setNotifications((curr) => {
      const next = markAllRead(curr);
      if (next !== curr) void saveNotifications(next);
      return next;
    });
  }, []);
  const allTags = useMemo(() => {
    const set = new Set<string>();
    for (const n of notes) for (const t of n.tags) set.add(t);
    return Array.from(set).sort();
  }, [notes]);

  const contentRef = useRef(content);
  const pathRef = useRef(path);
  // Mirror noteTitle in a ref so background callbacks (transcription
  // complete, reconcile complete) can label notification records
  // without taking the title as an explicit dep. Updated by an effect
  // alongside noteTitle's useMemo declaration further down.
  const noteTitleRef = useRef("");
  const savedRef = useRef(savedContent);
  const recentFilesRef = useRef<string[]>(recentFiles);
  const recordingRef = useRef<NoteRecording>(recording);
  const aiRef = useRef<AISettings>(aiSettings);
  const editorRef = useRef<ReactCodeMirrorRef>(null);
  const notesDirRef = useRef<string | null>(null);
  const tagsRef = useRef<string[]>(tags);
  const archivedRef = useRef<boolean>(archived);
  const favoriteRef = useRef<boolean>(favorite);
  const frontmatterExtrasRef = useRef<Record<string, unknown>>(frontmatterExtras);
  const notesScopeRef = useRef<"active" | "archived" | "favorites">(notesScope);
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
    archivedRef.current = archived;
  }, [archived]);
  useEffect(() => {
    favoriteRef.current = favorite;
  }, [favorite]);
  useEffect(() => {
    frontmatterExtrasRef.current = frontmatterExtras;
  }, [frontmatterExtras]);
  useEffect(() => {
    notesScopeRef.current = notesScope;
  }, [notesScope]);

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
   *  feed and the note-header tag autocomplete both read from this.
   *  Scopes by `notesScope` — Home toggles between active and archive views. */
  const refreshNotes = useCallback(async () => {
    try {
      const items = await listNotes(notesScopeRef.current);
      setNotes(items);
    } catch (err) {
      console.error("listNotes failed:", err);
    } finally {
      setNotesLoading(false);
    }
  }, []);

  /** Re-scan open action items. Used by the Home teaser, the sidebar
   *  count badge, and the dedicated actions feed. Always fetches the
   *  open scope; done/all are query-time choices the actions feed can
   *  surface later if we add a "Show completed" toggle. */
  const refreshActions = useCallback(async () => {
    try {
      const items = await listActions("open");
      setActions(items);
    } catch (err) {
      console.error("listActions failed:", err);
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
          setArchived(note.archived);
          setFavorite(note.favorite);
          setFrontmatterExtras(note.frontmatter_extras ?? {});
        } else {
          const file = await readFile(p);
          setPath(file.path);
          setContent(file.content);
          setSavedContent(file.content);
          setTags([]);
          setArchived(false);
          setFavorite(false);
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
    if (inFlightRef.current) return; // refuse a second op while another is running
    try {
      await startMeetingRecording(
        current,
        aiRef.current.recordSystemAudio,
        aiRef.current.glossary,
        aiRef.current.whisperModel,
      );
      setSysAvailable(true);
      const startedAt = Date.now();
      setRecording({ kind: "recording", startedAt });
      setInFlight({
        kind: "recording",
        notePath: current,
        noteTitle: noteTitleRef.current,
        startedAt,
      });
    } catch (err) {
      setRecording({
        kind: "error",
        message: typeof err === "string" ? err : "Failed to start recording.",
      });
    }
  }, []);

  const runTranscribe = useCallback(
    async (
      wavPath: string,
      snapshot: { notePath: string; noteTitle: string },
    ) => {
      setInFlight({
        kind: "transcribing",
        notePath: snapshot.notePath,
        noteTitle: snapshot.noteTitle,
        phase: "asr",
        pct: 0,
      });
      try {
        await transcribe(wavPath, aiRef.current.glossary, aiRef.current.whisperModel);
        const tp = transcriptPathFor(snapshot.notePath);

        // Notify regardless of where the user is now.
        pushNotificationAndPersist({
          kind: "transcription-complete",
          note_path: snapshot.notePath,
          note_title: snapshot.noteTitle,
        });

        // Refresh the per-note quiescent state only if the user is
        // still on the source note. Otherwise loadFile will pick up
        // the new transcript next time they navigate back.
        if (pathRef.current === snapshot.notePath) {
          setRecording({ kind: "ready", transcriptPath: tp });
        }
      } catch (err) {
        const message = typeof err === "string" ? err : "Transcription failed.";
        pushNotificationAndPersist({
          kind: "transcription-complete",
          note_path: snapshot.notePath,
          note_title: snapshot.noteTitle,
          body: message,
        });
        if (pathRef.current === snapshot.notePath) {
          setRecording({ kind: "error", message });
        }
      } finally {
        setInFlight((curr) =>
          curr &&
          curr.kind === "transcribing" &&
          curr.notePath === snapshot.notePath
            ? null
            : curr,
        );
      }
    },
    [pushNotificationAndPersist],
  );

  const onStopRecording = useCallback(async () => {
    if (recordingRef.current.kind !== "recording") return;
    const sourceNotePath = pathRef.current;
    const sourceNoteTitle = noteTitleRef.current;
    try {
      const wavPath = await stopMeetingRecording();
      // Reset the quiescent state now that recording has ended; the
      // transcription op takes over via inFlight.
      setRecording({ kind: "none", hasTranscript: false });
      if (sourceNotePath) {
        void runTranscribe(wavPath, {
          notePath: sourceNotePath,
          noteTitle: sourceNoteTitle,
        });
      }
    } catch (err) {
      setRecording({
        kind: "error",
        message: typeof err === "string" ? err : "Failed to stop recording.",
      });
      setInFlight((curr) => (curr && curr.kind === "recording" ? null : curr));
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
    // Drop the in-flight recording op too so a fresh recording can
    // start immediately after the discard.
    setInFlight((curr) => (curr && curr.kind === "recording" ? null : curr));
  }, []);

  const runReconcile = useCallback(
    async (transcriptPath: string) => {
      const notePath = pathRef.current;
      if (!notePath) return;

      // Snapshot every input we might need at completion time. Reading
      // refs after the await would otherwise pick up whichever note
      // the user has navigated to in the meantime — which would write
      // the source meeting's frontmatter with the wrong tags / archived
      // / favorite, and overwrite the visible buffer with reconciled
      // markdown belonging to a different note.
      const fallback = (notePath.split("/").pop() ?? "Untitled note").replace(
        /\.md$/,
        "",
      );
      const snapshot = {
        notePath,
        noteTitle: deriveTitle(contentRef.current, fallback),
        body: contentRef.current,
        tags: tagsRef.current,
        archived: archivedRef.current,
        favorite: favoriteRef.current,
        frontmatterExtras: frontmatterExtrasRef.current,
      };

      setInFlight({
        kind: "reconciling",
        notePath: snapshot.notePath,
        noteTitle: snapshot.noteTitle,
      });

      try {
        const md = await reconcileNotes(
          snapshot.body,
          transcriptPath,
          snapshot.noteTitle,
          aiRef.current.summaryModel,
          aiRef.current.glossary,
        );

        // Persist via the snapshotted side fields — never the live
        // refs.
        let nextSaved = md;
        try {
          if (isOwnedPath(snapshot.notePath)) {
            const result = await writeNote(
              snapshot.notePath,
              md,
              snapshot.tags,
              snapshot.archived,
              snapshot.favorite,
              snapshot.frontmatterExtras,
            );
            if (result.rewritten_body && result.rewritten_body !== md) {
              nextSaved = result.rewritten_body;
            }
          } else {
            await writeFile(snapshot.notePath, md);
          }
        } catch (err) {
          console.error("post-reconcile save failed:", err);
        }

        // Notification fires regardless of where the user is now.
        pushNotificationAndPersist({
          kind: "reconcile-complete",
          note_path: snapshot.notePath,
          note_title: snapshot.noteTitle,
        });

        // Visual updates ONLY when the user is still on the source
        // note. Otherwise loadFile picks up the new body next time
        // they navigate back.
        if (pathRef.current === snapshot.notePath) {
          const view = editorRef.current?.view;
          if (nextSaved !== snapshot.body && view) {
            dispatchDiff(view, snapshot.body, nextSaved);
          } else {
            setContent(nextSaved);
          }
          setSavedContent(nextSaved);
          setRecording({
            kind: "none",
            hasTranscript: true,
            transcriptPath,
            reconciled: true,
          });
        }
      } catch (err) {
        const message =
          typeof err === "string" ? err : "Reconciliation failed.";
        pushNotificationAndPersist({
          kind: "reconcile-complete",
          note_path: snapshot.notePath,
          note_title: snapshot.noteTitle,
          body: message,
        });
        if (pathRef.current === snapshot.notePath) {
          setRecording({
            kind: "error",
            message,
            transcriptPath,
          });
        }
      } finally {
        setInFlight((curr) =>
          curr &&
          curr.kind === "reconciling" &&
          curr.notePath === snapshot.notePath
            ? null
            : curr,
        );
      }
    },
    [isOwnedPath, pushNotificationAndPersist],
  );

  // Promise-based modal: resolves with the chosen member IDs, or null on
  // cancel. The picker is rendered conditionally below; submit/cancel
  // handlers fire `resolve` and clear pickerState.
  const askForAttendees = useCallback(
    (notePath: string) =>
      new Promise<string[] | null>((resolve) => {
        setPickerState({ notePath, resolve });
      }),
    [],
  );

  const requestTeamView = useCallback(() => {
    setMode("home");
    window.dispatchEvent(new CustomEvent("margin:nav", { detail: "team" }));
  }, []);

  const onGenerate = useCallback(async () => {
    // Refuse a second op while one is in flight. The user-facing path
    // here is rare — the Generate CTA only renders when the visible
    // banner is in a quiescent "ready" state — but a stale click during
    // the modal flow could otherwise queue a second reconcile.
    if (inFlightRef.current) return;
    const r = recordingRef.current;
    let tp: string | undefined;
    if (r.kind === "ready") tp = r.transcriptPath;
    else if (r.kind === "none" && r.hasTranscript) tp = r.transcriptPath;
    else if (r.kind === "error" && r.transcriptPath) tp = r.transcriptPath;
    if (!tp) return;
    const np = pathRef.current;
    if (!np) return;
    const ids = await askForAttendees(np);
    if (!ids) return; // user cancelled
    try {
      await setMeetingAttendees(np, ids);
      // Refresh the chip cluster so the header reflects the new list
      // immediately, before reconcile finishes.
      const fresh = await getMeetingAttendees(np);
      setMeetingAttendeesState(fresh);
    } catch (err) {
      console.error("setMeetingAttendees failed:", err);
      return;
    }
    void runReconcile(tp);
  }, [askForAttendees, runReconcile]);

  const onDismissError = useCallback(() => {
    void refreshRecordingState(pathRef.current);
  }, [refreshRecordingState]);

  // transcribe-progress + model-download-progress. These now write to
  // the in-flight slot so progress survives the user navigating away
  // from the source note mid-transcription.
  useEffect(() => {
    const unTr = listen<number>("transcribe-progress", (e) => {
      const pct = typeof e.payload === "number" ? e.payload : 0;
      setInFlight((curr) =>
        curr && curr.kind === "transcribing" ? { ...curr, pct } : curr,
      );
    });
    const unDl = listen<{ downloaded: number; total: number }>(
      "model-download-progress",
      (e) => {
        if (!e.payload) return;
        setInFlight((curr) =>
          curr && curr.kind === "transcribing"
            ? {
                ...curr,
                modelDl: {
                  downloaded: e.payload.downloaded,
                  total: e.payload.total,
                },
              }
            : curr,
        );
      },
    );
    const unPhase = listen<string>("transcribe-phase", (e) => {
      if (e.payload === "diarizing") {
        setInFlight((curr) =>
          curr && curr.kind === "transcribing"
            ? { ...curr, phase: "diar", modelDl: undefined }
            : curr,
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

  // Listeners for the in-app notifications panel (#37). Reconcile
  // emits "reconcile-progress" with payload "done"; reminders emit
  // a structured object on "notification:reminder". Transcription
  // completion is pushed directly from `runTranscribe` (it has the
  // resolved note path in scope).
  useEffect(() => {
    const unRec = listen<string>("reconcile-progress", (e) => {
      if (e.payload !== "done") return;
      const np = pathRef.current;
      if (!np) return;
      pushNotificationAndPersist({
        kind: "reconcile-complete",
        note_path: np,
        note_title: noteTitleRef.current,
      });
    });
    const unRem = listen<{
      action_id?: string;
      note_path?: string;
      note_title?: string;
      action_text?: string;
    }>("notification:reminder", (e) => {
      const p = e.payload;
      if (!p || !p.note_path || !p.note_title) return;
      pushNotificationAndPersist({
        kind: "action-item-reminder",
        note_path: p.note_path,
        note_title: p.note_title,
        action_id: p.action_id,
        body: p.action_text,
      });
    });
    return () => {
      unRec.then((u) => u());
      unRem.then((u) => u());
    };
  }, [pushNotificationAndPersist]);

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
      const startedAt = Date.now();
      setRecording({ kind: "recording", startedAt });
      setInFlight({
        kind: "recording",
        notePath: ref.note_path,
        noteTitle: noteTitleRef.current,
        startedAt,
      });
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
      const before = contentRef.current;
      let nextSaved = before;
      if (isOwnedPath(target)) {
        const result = await writeNote(
          target,
          before,
          tagsRef.current,
          archivedRef.current,
          favoriteRef.current,
          frontmatterExtrasRef.current,
        );
        if (result.rewritten_body && result.rewritten_body !== before) {
          const view = editorRef.current?.view;
          if (view) {
            dispatchDiff(view, before, result.rewritten_body);
          } else {
            setContent(result.rewritten_body);
          }
          nextSaved = result.rewritten_body;
        }
      } else {
        await writeFile(target, before);
      }
      setPath(target);
      setSavedContent(nextSaved);
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
        let nextSaved = content;
        if (isOwnedPath(path)) {
          const result = await writeNote(
            path,
            content,
            tagsRef.current,
            archivedRef.current,
            favoriteRef.current,
            frontmatterExtrasRef.current,
          );
          // Rust may have rewritten relative due-date tokens
          // (`@today`/`@tomorrow`/`@<weekday>`) to absolute ISO. Push the
          // narrow diff straight at the editor view so CodeMirror's
          // selection-mapping preserves cursor and scroll; a full-doc
          // setContent here would collapse selection to 0 and jump the
          // viewport to the top.
          if (result.rewritten_body && result.rewritten_body !== content) {
            const view = editorRef.current?.view;
            if (view) {
              dispatchDiff(view, content, result.rewritten_body);
            } else {
              setContent(result.rewritten_body);
            }
            nextSaved = result.rewritten_body;
          }
        } else {
          await writeFile(path, content);
        }
        setSavedContent(nextSaved);
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
        let nextArchived: boolean | null = null;
        let nextFavorite: boolean | null = null;
        let nextExtras: Record<string, unknown> | null = null;
        if (isOwnedPath(e.payload)) {
          const note = await readNote(e.payload);
          nextBody = note.body;
          nextTags = note.tags;
          nextArchived = note.archived;
          nextFavorite = note.favorite;
          nextExtras = note.frontmatter_extras ?? {};
        } else {
          const f = await readFile(e.payload);
          nextBody = f.content;
        }
        if (nextBody === savedRef.current) {
          // The body didn't change. Pick up any tag-only edits made via
          // an external editor and move on without disturbing the buffer.
          if (nextTags) setTags(nextTags);
          if (nextArchived !== null) setArchived(nextArchived);
          if (nextFavorite !== null) setFavorite(nextFavorite);
          if (nextExtras) setFrontmatterExtras(nextExtras);
          return;
        }
        if (contentRef.current === savedRef.current) {
          setContent(nextBody);
          setSavedContent(nextBody);
          if (nextTags) setTags(nextTags);
          if (nextArchived !== null) setArchived(nextArchived);
          if (nextFavorite !== null) setFavorite(nextFavorite);
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
        setArchived(note.archived);
        setFavorite(note.favorite);
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
    void refreshActions();
  }, []);

  // Re-scan notes whenever the user enters home or flips between active
  // and archive scopes so the feed reflects any edits since last visit.
  // Also refresh actions so the teaser + sidebar count stay current.
  useEffect(() => {
    if (mode === "home") {
      void refreshNotes();
      void refreshActions();
    }
    // refreshNotes is stable (defined below with empty deps)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode, notesScope]);

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
        setArchived(false);
        setFavorite(false);
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

  /** Toggle a note's archived flag.
   *  - `target` defaults to the open note when omitted.
   *  - `nextArchived` defaults to flipping the current state for the open
   *    note. Row callers should always pass it explicitly since they may
   *    not know the per-row flag (in this UI, scope already implies it).
   */
  const onArchiveNote = useCallback(
    async (target?: string, nextArchived?: boolean) => {
      const note = target ?? pathRef.current;
      if (!note || !isOwnedPath(note)) return;
      const wasOpen = note === pathRef.current;
      const next =
        nextArchived ?? (wasOpen ? !archivedRef.current : true);

      try {
        await setArchivedFile(note, next);
      } catch (err) {
        console.error("setArchived failed:", err);
        alert("Could not update archive state. See console for details.");
        return;
      }

      if (wasOpen) {
        setArchived(next);
      }

      await refreshNotes();

      // If we archived the currently-open note while in active scope (or
      // un-archived it while in archive scope), it no longer belongs in the
      // current view — kick back to home so the user isn't stranded.
      if (wasOpen) {
        const scopeMatchesNew =
          (next && notesScopeRef.current === "archived") ||
          (!next && notesScopeRef.current === "active");
        if (!scopeMatchesNew) setMode("home");
      }
    },
    [isOwnedPath, refreshNotes],
  );

  /** Toggle a note's favorite flag.
   *  - `target` defaults to the open note.
   *  - `nextFavorited` defaults to flipping current state for the open
   *    note. Row callers should always pass it explicitly.
   */
  const onFavoriteNote = useCallback(
    async (target?: string, nextFavorited?: boolean) => {
      const note = target ?? pathRef.current;
      if (!note || !isOwnedPath(note)) return;
      const wasOpen = note === pathRef.current;
      const next =
        nextFavorited ?? (wasOpen ? !favoriteRef.current : true);

      try {
        await setFavoriteFile(note, next);
      } catch (err) {
        console.error("setFavorite failed:", err);
        alert("Could not update favorite state. See console for details.");
        return;
      }

      if (wasOpen) {
        setFavorite(next);
      }

      await refreshNotes();

      // If the open note no longer matches the favorites scope, kick to
      // home. (Active scope keeps un-favorited notes; only the favorites
      // view filters them out.)
      if (wasOpen && notesScopeRef.current === "favorites" && !next) {
        setMode("home");
      }
    },
    [isOwnedPath, refreshNotes],
  );

  /** Clone a note to a new bundle and open it. `target` defaults to the
   *  open note. Doesn't copy audio.wav / transcript.json — see the
   *  `duplicate_note` rationale in notes.rs. */
  const onDuplicateNote = useCallback(
    async (target?: string) => {
      const source = target ?? pathRef.current;
      if (!source || !isOwnedPath(source)) return;
      let ref;
      try {
        ref = await duplicateNote(source);
      } catch (err) {
        console.error("duplicateNote failed:", err);
        alert("Could not duplicate the note. See console for details.");
        return;
      }
      await refreshNotes();
      await loadFile(ref.note_path);
    },
    [isOwnedPath, refreshNotes, loadFile],
  );

  /** Toggle the done state of an action item. Optimistic — flips the
   *  local state instantly, then writes through. On error, revert and
   *  surface the message.
   *
   *  We deliberately keep the row in the list after marking it done,
   *  so the user can see what they just completed (filled checkbox +
   *  strikethrough) instead of having it vanish mid-click. The next
   *  navigation back into Home triggers `refreshActions()` which
   *  re-fetches the open scope and drops the now-done row naturally. */
  /** Append a quick todo to the catch-all Inbox bundle and refresh the
   *  actions feed. The Inbox bundle is find-or-created on the Rust side
   *  by `ensure_inbox_note`. The action goes through the normal write
   *  path, so any relative date token gets resolved to absolute on save. */
  const onAddInboxTodo = useCallback(
    async (text: string, dueToken: string | null) => {
      try {
        const ref = await ensureInboxNote();
        const note = await readNote(ref.note_path);
        const trimmedBody = note.body.replace(/\s+$/, "");
        const sep = trimmedBody.length === 0 ? "" : "\n\n";
        const dueSuffix = dueToken && dueToken.trim() ? ` @${dueToken.trim()}` : "";
        const nextBody = `${trimmedBody}${sep}- [ ] ${text}${dueSuffix}\n`;
        await writeNote(
          ref.note_path,
          nextBody,
          note.tags,
          note.archived,
          note.favorite,
          note.frontmatter_extras,
        );
        await refreshActions();
      } catch (err) {
        console.error("onAddInboxTodo failed:", err);
        alert("Could not add the todo. See console for details.");
      }
    },
    [refreshActions],
  );

  const onToggleAction = useCallback(async (id: string, nextDone: boolean) => {
    setActions((curr) =>
      curr.map((a) => (a.id === id ? { ...a, done: nextDone } : a)),
    );
    try {
      await setActionDone(id, nextDone);
    } catch (err) {
      console.error("setActionDone failed:", err);
      alert("Could not update the task. See console for details.");
      setActions((curr) =>
        curr.map((a) => (a.id === id ? { ...a, done: !nextDone } : a)),
      );
    }
  }, []);

  // Slice of actions belonging to the currently-open note (#53). The
  // Editor uses this to decorate inline checkbox lines with the
  // assignee chip. Empty when no note is open.
  const noteActions = useMemo(
    () => (path ? actions.filter((a) => a.note_path === path) : []),
    [actions, path],
  );

  // Refresh actions whenever the on-disk saved content changes for the
  // current note. Catches autosave, manual save, and the reconcile
  // write path so the inline chips (#53) pick up freshly-resolved
  // assignee_ids without a separate notification.
  useEffect(() => {
    if (!path) return;
    void refreshActions();
  }, [path, savedContent, refreshActions]);

  // Refresh team-member list. Cheap; runs on mount and on every
  // `margin:nav` event (fires when the user changes sidebar nav).
  useEffect(() => {
    let cancelled = false;
    const refresh = async () => {
      try {
        const fresh = await listTeamMembers();
        if (!cancelled) setMembers(fresh);
      } catch (err) {
        console.error("listTeamMembers failed:", err);
      }
    };
    void refresh();
    const onNav = () => void refresh();
    const onTeamChanged = () => void refresh();
    window.addEventListener("margin:nav", onNav);
    window.addEventListener("margin:team-changed", onTeamChanged);
    return () => {
      cancelled = true;
      window.removeEventListener("margin:nav", onNav);
      window.removeEventListener("margin:team-changed", onTeamChanged);
    };
  }, []);

  // Reassign an action item to a new team member (or unassign with
  // null). Optimistic local update first; on success refetch the list
  // because the action's id changes when the body line is rewritten.
  // On error, refetch to restore authoritative state.
  const onReassignAction = useCallback(
    async (actionId: string, memberId: string | null) => {
      const newName =
        memberId === null
          ? null
          : members.find((m) => m.id === memberId)?.display_name ?? null;
      setActions((curr) =>
        curr.map((a) =>
          a.id === actionId
            ? { ...a, assignee_id: memberId, assignee_display_name: newName }
            : a,
        ),
      );
      try {
        await setActionAssignee(actionId, memberId);
        const fresh = await listActions("open");
        setActions(fresh);
      } catch (err) {
        console.error("setActionAssignee failed:", err);
        alert("Could not change the owner. See console for details.");
        try {
          const fresh = await listActions("open");
          setActions(fresh);
        } catch {}
      }
    },
    [members],
  );

  /** Open the macOS share sheet for the active note. The Rust side
   *  writes a renamed temp `<title>.md` (frontmatter stripped) and
   *  hands the file URL to NSSharingServicePicker. */
  const onShareNote = useCallback(async () => {
    const target = pathRef.current;
    if (!target || !isOwnedPath(target)) return;
    try {
      await shareNote(target);
    } catch (err) {
      console.error("shareNote failed:", err);
      alert("Could not open the share sheet. See console for details.");
    }
  }, [isOwnedPath]);

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
  useEffect(() => {
    noteTitleRef.current = noteTitle;
  }, [noteTitle]);

  // Has the active note's transcript already been reconciled at least
  // once? Read from transcript.json's `reconciled_at` field via the
  // recording state machine. Used to suppress the post-recording
  // Generate-notes CTA after the user has run it.
  const notesGenerated = recording.kind === "none" && !!recording.reconciled;

  // Banner shows the in-flight op when one is running for the
  // currently-displayed note, otherwise the per-note quiescent
  // recording state. Decoupling these is what makes navigating away
  // during transcription / reconcile safe — neither slot writes to the
  // other.
  const bannerState = useMemo<NoteRecording>(() => {
    if (inFlight && inFlight.notePath === path) {
      if (inFlight.kind === "recording") {
        return { kind: "recording", startedAt: inFlight.startedAt };
      }
      if (inFlight.kind === "transcribing") {
        return {
          kind: "transcribing",
          phase: inFlight.phase,
          pct: inFlight.pct,
          modelDl: inFlight.modelDl,
        };
      }
      return { kind: "reconciling" };
    }
    return recording;
  }, [inFlight, recording, path]);

  const onTitleChange = useCallback((next: string) => {
    setContent((cur) => rewriteH1(cur, next));
  }, []);

  const onEditorPrefsChange = useCallback(
    (next: {
      tabSize: number;
      useTabs: boolean;
      softWrap: boolean;
      fontSize: number;
    }) => {
      setTabSize(next.tabSize);
      setUseTabs(next.useTabs);
      setSoftWrap(next.softWrap);
      setFontSize(next.fontSize);
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
          onArchive={
            isOwned && recording.kind === "none"
              ? () => void onArchiveNote()
              : undefined
          }
          archived={archived}
          onFavorite={
            isOwned && recording.kind === "none"
              ? () => void onFavoriteNote()
              : undefined
          }
          favorited={favorite}
          onDuplicate={
            isOwned && recording.kind === "none"
              ? () => void onDuplicateNote()
              : undefined
          }
          onShare={
            isOwned && recording.kind === "none"
              ? () => void onShareNote()
              : undefined
          }
          onSummarize={
            isOwned &&
            (recording.kind === "ready" ||
              (recording.kind === "none" && !!recording.hasTranscript) ||
              (recording.kind === "error" && !!recording.transcriptPath))
              ? () => void onGenerate()
              : undefined
          }
          attendees={meetingAttendees}
          onEditAttendees={
            isOwned &&
            (recording.kind === "ready" ||
              (recording.kind === "none" && !!recording.hasTranscript) ||
              (recording.kind === "error" && !!recording.transcriptPath))
              ? () => void onGenerate()
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
          state={bannerState}
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
            fontSize={fontSize}
            actions={noteActions}
          />
        )}
        {mode === "preview" && (
          <Preview source={content} theme={theme} onSourceChange={setContent} />
        )}
        {mode === "transcript" && transcriptPath && (
          <TranscriptView path={transcriptPath} />
        )}
        {mode === "settings" && (
          <Settings
            theme={themeSettings}
            ai={aiSettings}
            editor={{ tabSize, useTabs, softWrap, fontSize }}
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
            scope={notesScope}
            onScopeChange={setNotesScope}
            onOpen={(p) => void loadFile(p)}
            onNewNote={() => void onNewNote()}
            onNewMeeting={() => void onNewMeeting()}
            onOpenSettings={() => tryNavigate("settings")}
            onDeleteRow={(p) => void onDeleteNote(p)}
            onArchiveRow={(p, next) => void onArchiveNote(p, next)}
            onFavoriteRow={(p, next) => void onFavoriteNote(p, next)}
            onDuplicateRow={(p) => void onDuplicateNote(p)}
            actions={actions}
            onToggleAction={(id, next) => void onToggleAction(id, next)}
            onAddInboxTodo={onAddInboxTodo}
            editor={{ tabSize, useTabs, softWrap, fontSize }}
            members={members}
            onReassignAction={onReassignAction}
            notifications={notifications}
            onMarkAllNotificationsRead={markNotificationsRead}
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

      <DueDatePopover />
      <AssigneePopover
        members={members}
        onPick={(actionId, memberId) => onReassignAction(actionId, memberId)}
      />
      {pickerState && (
        <AttendeePicker
          notePath={pickerState.notePath}
          onSubmit={(ids) => {
            const r = pickerState.resolve;
            setPickerState(null);
            r(ids);
          }}
          onCancel={() => {
            const r = pickerState.resolve;
            setPickerState(null);
            r(null);
          }}
          onAddTeamMember={() => {
            const r = pickerState.resolve;
            setPickerState(null);
            r(null);
            requestTeamView();
          }}
        />
      )}
    </div>
  );
}

