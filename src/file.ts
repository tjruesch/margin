import { invoke } from "@tauri-apps/api/core";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";

export type FileContents = { path: string; content: string };

const MD_FILTER = [{ name: "Markdown", extensions: ["md", "markdown", "mdown", "mkd", "mkdn"] }];

export async function pickFileToOpen(): Promise<string | null> {
  const result = await openDialog({ multiple: false, filters: MD_FILTER });
  if (typeof result === "string") return result;
  return null;
}

export async function pickFileToSave(suggestedName = "Untitled.md"): Promise<string | null> {
  const result = await saveDialog({ defaultPath: suggestedName, filters: MD_FILTER });
  return result ?? null;
}

export async function readFile(path: string): Promise<FileContents> {
  return invoke<FileContents>("read_file", { path });
}

export async function writeFile(path: string, content: string): Promise<void> {
  await invoke<void>("write_file", { path, content });
}

export async function getInitialFile(): Promise<string | null> {
  const p = await invoke<string | null>("initial_file");
  return p ?? null;
}

export async function watchFile(path: string): Promise<void> {
  await invoke<void>("watch_file", { path });
}

export async function unwatchFile(): Promise<void> {
  await invoke<void>("unwatch_file");
}

export async function hasAnthropicApiKey(): Promise<boolean> {
  return invoke<boolean>("has_anthropic_api_key");
}

export async function setAnthropicApiKey(key: string): Promise<void> {
  await invoke<void>("set_anthropic_api_key", { key });
}

export async function deleteAnthropicApiKey(): Promise<void> {
  await invoke<void>("delete_anthropic_api_key");
}

// --- Notes (bundle abstraction) ------------------------------------------

export type NoteRef = { id: string; note_path: string };

export type NoteListItem = {
  note_path: string;
  title: string;
  modified_ms: number;
  duration_ms: number | null;
  preview: string;
  tags: string[];
  favorite: boolean;
};

export type NoteContent = {
  body: string;
  tags: string[];
  archived: boolean;
  favorite: boolean;
  /** Frontmatter keys other than `tags`/`archived`/`favorite`, opaque to
   *  the frontend. Round-trip unchanged on writes so user-added YAML
   *  survives. */
  frontmatter_extras: Record<string, unknown>;
};

export async function readNote(notePath: string): Promise<NoteContent> {
  return invoke<NoteContent>("read_note", { notePath });
}

export async function writeNote(
  notePath: string,
  body: string,
  tags: string[],
  archived: boolean,
  favorite: boolean,
  frontmatterExtras: Record<string, unknown>,
): Promise<void> {
  await invoke<void>("write_note", {
    notePath,
    body,
    tags,
    archived,
    favorite,
    frontmatterExtras,
  });
}

export async function setNoteTags(notePath: string, tags: string[]): Promise<void> {
  await invoke<void>("set_note_tags", { notePath, tags });
}

export async function setArchived(notePath: string, archived: boolean): Promise<void> {
  await invoke<void>("set_archived", { notePath, archived });
}

export async function setFavorite(notePath: string, favorite: boolean): Promise<void> {
  await invoke<void>("set_favorite", { notePath, favorite });
}

export async function shareNote(notePath: string): Promise<void> {
  await invoke<void>("share_note", { notePath });
}

export async function notesDir(): Promise<string> {
  return invoke<string>("notes_dir");
}

export async function createNote(): Promise<NoteRef> {
  return invoke<NoteRef>("create_note");
}

export async function convertExternal(sourcePath: string): Promise<NoteRef> {
  return invoke<NoteRef>("convert_external", { sourcePath });
}

export async function duplicateNote(notePath: string): Promise<NoteRef> {
  return invoke<NoteRef>("duplicate_note", { notePath });
}

export async function isOwnedNote(path: string): Promise<boolean> {
  return invoke<boolean>("is_owned_note", { path });
}

export type NoteScope = "active" | "archived" | "favorites" | "all";

export async function listNotes(scope: NoteScope = "active"): Promise<NoteListItem[]> {
  return invoke<NoteListItem[]>("list_notes", { scope });
}

export type NoteMeta = { modified_ms: number };

export async function noteMeta(notePath: string): Promise<NoteMeta> {
  return invoke<NoteMeta>("note_meta", { notePath });
}

export async function discardRecording(notePath: string): Promise<void> {
  await invoke<void>("discard_recording", { notePath });
}

export async function deleteNote(notePath: string): Promise<void> {
  await invoke<void>("delete_note", { notePath });
}

// --- Recording + transcription -------------------------------------------

export type Segment = {
  start_ms: number;
  end_ms: number;
  text: string;
  speaker?: number | null;
};
export type Transcript = {
  segments: Segment[];
  full_text: string;
  language: string;
  duration_ms: number;
  num_speakers?: number | null;
  /** Unix-ms timestamp of the last successful Claude reconcile against
   *  this transcript. `null`/missing means the user hasn't generated
   *  notes from it yet. */
  reconciled_at?: number | null;
};

export async function startMeetingRecording(
  notePath: string,
  withSystemAudio = false,
): Promise<string> {
  return invoke<string>("start_meeting_recording", { notePath, withSystemAudio });
}

export async function stopMeetingRecording(): Promise<string> {
  return invoke<string>("stop_meeting_recording");
}

export async function transcribe(
  audioPath: string,
  glossary: string[] = [],
  model?: string,
): Promise<Transcript> {
  return invoke<Transcript>("transcribe", { audioPath, glossary, model });
}

export async function reconcileNotes(
  handNotes: string,
  transcriptPath: string,
  title: string,
  model?: string,
  glossary: string[] = [],
): Promise<string> {
  return invoke<string>("reconcile_notes", {
    handNotes,
    transcriptPath,
    title,
    model,
    glossary,
  });
}
