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
};

export async function notesDir(): Promise<string> {
  return invoke<string>("notes_dir");
}

export async function createNote(): Promise<NoteRef> {
  return invoke<NoteRef>("create_note");
}

export async function convertExternal(sourcePath: string): Promise<NoteRef> {
  return invoke<NoteRef>("convert_external", { sourcePath });
}

export async function isOwnedNote(path: string): Promise<boolean> {
  return invoke<boolean>("is_owned_note", { path });
}

export async function listNotes(): Promise<NoteListItem[]> {
  return invoke<NoteListItem[]>("list_notes");
}

export async function discardRecording(notePath: string): Promise<void> {
  await invoke<void>("discard_recording", { notePath });
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
): Promise<Transcript> {
  return invoke<Transcript>("transcribe", { audioPath, glossary });
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
