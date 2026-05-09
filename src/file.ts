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

/** When the Rust side rewrites relative due-date tokens (`@today`,
 *  `@tomorrow`, `@<weekday>`) to their absolute `@YYYY-MM-DD` forms,
 *  `rewritten_body` carries the new body so the editor can swap its
 *  in-memory text to match disk. `null` if no rewrite happened. */
export type WriteNoteResult = {
  rewritten_body: string | null;
};

export async function writeNote(
  notePath: string,
  body: string,
  tags: string[],
  archived: boolean,
  favorite: boolean,
  frontmatterExtras: Record<string, unknown>,
): Promise<WriteNoteResult> {
  return invoke<WriteNoteResult>("write_note", {
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

// --- Action items --------------------------------------------------------

export type ActionScope = "open" | "done" | "all";

export type ActionListItem = {
  id: string;
  note_path: string;
  note_title: string;
  text: string;
  done: boolean;
  line: number;
  created_ms: number;
  /** Absolute due-date timestamp (Unix ms) parsed from a trailing
   *  `@YYYY-MM-DD[ HH:MM]` token. `null` means the action has no due date. */
  due_ms: number | null;
  /** team_members.id when the leading "Owner — " segment in the action's
   *  text matched exactly one team member (#49), else `null`. */
  assignee_id: string | null;
  /** Canonical display name from team_members, joined for render so the
   *  frontend can show an avatar chip without a second IPC round-trip. */
  assignee_display_name: string | null;
};

export async function listActions(
  scope: ActionScope = "open",
  assigneeId?: string,
): Promise<ActionListItem[]> {
  return invoke<ActionListItem[]>("list_actions", { scope, assigneeId });
}

export async function setActionDone(id: string, done: boolean): Promise<void> {
  await invoke<void>("set_action_done", { id, done });
}

export async function notesDir(): Promise<string> {
  return invoke<string>("notes_dir");
}

export async function createNote(): Promise<NoteRef> {
  return invoke<NoteRef>("create_note");
}

/** Find-or-create the catch-all "Inbox" bundle that holds quick todos
 *  added from the Action items page. Stable bundle id so subsequent
 *  calls return the same NoteRef. */
export async function ensureInboxNote(): Promise<NoteRef> {
  return invoke<NoteRef>("ensure_inbox_note");
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

// --- Search (#31) --------------------------------------------------------

export type SearchSource = "title" | "body" | "transcript";

/** One ranked result from `search_notes`. The Rust side wraps the matched
 *  span in U+2068 / U+2069 (FSI/PDI) inside `snippet` so the UI can split
 *  on those marks to render the highlight without HTML round-tripping. */
export type SearchHit = {
  note_path: string;
  bundle_id: string;
  title: string;
  modified_ms: number;
  snippet: string;
  source: SearchSource;
  score: number;
};

export const SEARCH_HIGHLIGHT_OPEN = "\u{2068}";
export const SEARCH_HIGHLIGHT_CLOSE = "\u{2069}";

export async function searchNotes(query: string, limit = 20): Promise<SearchHit[]> {
  return invoke<SearchHit[]>("search_notes", { query, limit });
}

// --- AI Q&A (#31 follow-up) ---------------------------------------------

export type AskSource = {
  /** 1-based label the model is told to cite as `[N]`. */
  index: number;
  note_path: string;
  bundle_id: string;
  title: string;
  modified_ms: number;
};

export type ChatTurn = {
  role: "user" | "assistant";
  content: string;
};

/** Discriminated union pushed by the Rust side on the `ai-stream` Tauri
 *  event channel. Order: `sources` (once), then any number of `delta` /
 *  `tool_use_start` / `tool_use_done` interleaved (in the order the
 *  model emits text vs tool calls), then a terminal `done` or `error`.
 *  Filter by `turn_id` to ignore stale turns. */
export type AiStreamEvent =
  | { kind: "sources"; turn_id: string; sources: AskSource[] }
  | { kind: "delta"; turn_id: string; text: string }
  | {
      kind: "tool_use_start";
      turn_id: string;
      tool_id: string;
      name: string;
      target_n: number;
      target_title: string;
    }
  | { kind: "tool_use_done"; turn_id: string; tool_id: string; ok: boolean }
  | { kind: "done"; turn_id: string }
  | { kind: "error"; turn_id: string; message: string };

/** One ordered piece of an assistant message. The Rust side only emits
 *  text deltas + tool-use markers; the UI builds this list in arrival
 *  order so tool pills land at their position in the prose. */
export type MessagePart =
  | { kind: "text"; value: string }
  | {
      kind: "tool";
      toolId: string;
      name: string;
      targetN: number;
      targetTitle: string;
      status: "running" | "ok" | "error";
    };

/** Caller generates `turnId` (UUID) so the in-flight assistant message
 *  can be tagged with it before the first `ai-stream` event arrives —
 *  the backend's `Sources` emit can fire before invoke's promise
 *  resolves, and the listener needs the tag to associate the event
 *  with the right message. */
export async function askNotesStart(
  turnId: string,
  query: string,
  history: ChatTurn[] = [],
  model?: string,
): Promise<void> {
  return invoke<void>("ask_notes_start", { turnId, query, history, model });
}

// --- Voice mode (#57) ----------------------------------------------------

/** Result of a voice-query stop. `ok` carries the transcribed text;
 *  `silent` means the recording was below the silence threshold (no
 *  speech detected); `error` carries a user-facing message in `text`. */
export type VoiceTranscript = {
  status: "ok" | "silent" | "error";
  text: string;
};

export async function startVoiceRecording(): Promise<void> {
  return invoke<void>("start_voice_recording");
}

export async function stopVoiceRecording(model?: string): Promise<VoiceTranscript> {
  return invoke<VoiceTranscript>("stop_voice_recording", { model });
}

// --- Connectors (#59) ----------------------------------------------------

/** One connector + its current sync state. Returned by `list_connectors`,
 *  joined from the `connectors` and `sync_status` tables. */
export type ConnectorInfo = {
  id: string;
  kind: string;
  display_name: string;
  enabled: boolean;
  last_sync_ms: number | null;
  last_success_ms: number | null;
  last_error: string | null;
  next_due_ms: number;
};

export async function listConnectors(): Promise<ConnectorInfo[]> {
  return invoke<ConnectorInfo[]>("list_connectors");
}

/** Pushed by the Rust side on the `connector-status` Tauri event channel
 *  whenever a connector starts/finishes/errors a sync, or is added /
 *  removed. Consumers refetch via `listConnectors()` on each event to
 *  pick up the new state. */
export type ConnectorStatusEvent = {
  connector_id: string;
  state: "syncing" | "synced" | "errored" | "skipped" | "added" | "removed";
  message?: string;
};

/** A configured OAuth provider that the user can pick from in the
 *  "Add connector" modal. Only providers whose client ID is set at
 *  build time appear in this list. */
export type OAuthProviderInfo = {
  kind: string;
  display_name: string;
};

export async function listOAuthProviders(): Promise<OAuthProviderInfo[]> {
  return invoke<OAuthProviderInfo[]>("list_oauth_providers");
}

/** Run the OAuth flow for `kind`. Opens the system browser; returns
 *  the new (or updated) connector id when the user completes the
 *  grant. Rejects with the provider/connector error message if the
 *  user denies, the flow times out, or the network fails. */
export async function startOAuthConnector(kind: string): Promise<string> {
  return invoke<string>("start_oauth_connector", { kind });
}

export async function deleteConnector(connectorId: string): Promise<void> {
  return invoke<void>("delete_connector", { connectorId });
}

// --- Calendar events (#63) -----------------------------------------------

export type CalendarAttendee = {
  email: string;
  display_name: string | null;
  response_status: string | null;
  is_self: boolean;
  is_organizer: boolean;
  team_member_id: string | null;
};

export type CalendarEvent = {
  id: string;
  connector_id: string;
  external_id: string;
  title: string;
  start_ms: number;
  end_ms: number;
  all_day: boolean;
  location: string | null;
  description: string | null;
  source_calendar: string | null;
  status: string | null;
  raw_etag: string | null;
  modified_ms: number;
  /** Path to the note bundle the user linked to this event (set on
   *  first click of the event card). Null until linked. Survives
   *  re-syncs of the same event. */
  linked_note_path: string | null;
  attendees: CalendarAttendee[];
};

/** Read calendar events whose start time falls in [startMs, endMs].
 *  Optional `connectorId` to scope to a single source. The backend
 *  joins attendees in a single query; results are ordered by start
 *  time ascending. */
export async function listCalendarEvents(
  startMs: number,
  endMs: number,
  connectorId?: string,
): Promise<CalendarEvent[]> {
  return invoke<CalendarEvent[]>("list_calendar_events", {
    startMs,
    endMs,
    connectorId,
  });
}

export async function getEventDetails(eventId: string): Promise<CalendarEvent | null> {
  return invoke<CalendarEvent | null>("get_event_details", { eventId });
}

/** Returns a path to the note bundle for this calendar event. If the
 *  event was already linked, returns the existing path. Otherwise
 *  creates a fresh bundle with calendar metadata in the frontmatter,
 *  records meeting attendees in the team module, and persists the
 *  link on the event row. Used by the "Coming up" strip click
 *  handler (#62). */
export async function openOrCreateEventNote(eventId: string): Promise<string> {
  return invoke<string>("open_or_create_event_note", { eventId });
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

// --- Team members --------------------------------------------------------

export type TeamMember = {
  id: string;
  display_name: string;
  role: string;
  aliases: string[];
  profile_md_path: string;
  is_self: boolean;
  created_ms: number;
  updated_ms: number;
};

export async function listTeamMembers(): Promise<TeamMember[]> {
  return invoke<TeamMember[]>("list_team_members");
}

export async function getTeamMember(id: string): Promise<TeamMember> {
  return invoke<TeamMember>("get_team_member", { id });
}

export async function createTeamMember(
  displayName: string,
  role: string,
  aliases: string[],
): Promise<TeamMember> {
  return invoke<TeamMember>("create_team_member", { displayName, role, aliases });
}

export async function updateTeamMember(
  id: string,
  fields: { displayName?: string; role?: string; aliases?: string[] },
): Promise<TeamMember> {
  return invoke<TeamMember>("update_team_member", { id, ...fields });
}

export async function deleteTeamMember(id: string): Promise<void> {
  await invoke<void>("delete_team_member", { id });
}

export async function setMeetingAttendees(
  notePath: string,
  memberIds: string[],
): Promise<void> {
  await invoke<void>("set_meeting_attendees", { notePath, memberIds });
}

export async function getMeetingAttendees(notePath: string): Promise<TeamMember[]> {
  return invoke<TeamMember[]>("get_meeting_attendees", { notePath });
}

export async function setActionAssignee(
  actionId: string,
  memberId: string | null,
): Promise<void> {
  await invoke<void>("set_action_assignee", { actionId, memberId });
}

// --- Recording + transcription -------------------------------------------

export type AudioSource = "mic" | "system";

export type Segment = {
  start_ms: number;
  end_ms: number;
  text: string;
  speaker?: number | null;
  /** Dominant audio channel during this segment's chunk window (#47). Hint
   *  for who's speaking; not authoritative. `null`/missing on legacy
   *  transcripts and on the whole-WAV fallback path. */
  source?: AudioSource | null;
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
  glossary: string[] = [],
  model?: string,
): Promise<string> {
  return invoke<string>("start_meeting_recording", {
    notePath,
    withSystemAudio,
    glossary,
    model,
  });
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
