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

export async function meetingsDir(): Promise<string> {
  return invoke<string>("meetings_dir");
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

// --- Meeting recording + transcription -----------------------------------

export type Segment = { start_ms: number; end_ms: number; text: string };
export type Transcript = {
  segments: Segment[];
  full_text: string;
  language: string;
  duration_ms: number;
};

export async function startMeetingRecording(
  title: string,
  withSystemAudio = false,
): Promise<string> {
  return invoke<string>("start_meeting_recording", { title, withSystemAudio });
}

export async function stopMeetingRecording(): Promise<string> {
  return invoke<string>("stop_meeting_recording");
}

export async function deleteMeetingFiles(id: string): Promise<void> {
  await invoke<void>("delete_meeting_files", { id });
}

export async function transcribe(audioPath: string): Promise<Transcript> {
  return invoke<Transcript>("transcribe", { audioPath });
}

export async function summarizeMeeting(
  transcriptPath: string,
  title: string,
  model?: string,
): Promise<string> {
  return invoke<string>("summarize_meeting", { transcriptPath, title, model });
}
