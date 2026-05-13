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

// Filesystem-level IPCs (`readFile` / `writeFile` / `watchFile` /
// `unwatchFile` / `fileExists`) were removed in #112. The stubs
// below preserve the legacy call shape used by the editor's transcript
// loader and a couple of vestigial code paths so the migration diff
// stays focused on the architectural move. They are pure shims that
// proxy disk reads via Tauri's plugin-fs where it still makes sense
// (audio/transcript siblings).

export async function readFile(path: string): Promise<FileContents> {
  // After #112 this is only used to read transcript.json sidecars from
  // disk. The shim keeps existing call sites working without a wider
  // refactor.
  const { readTextFile } = await import("@tauri-apps/plugin-fs");
  const content = await readTextFile(path);
  return { path, content };
}

export async function writeFile(path: string, content: string): Promise<void> {
  const { writeTextFile } = await import("@tauri-apps/plugin-fs");
  await writeTextFile(path, content);
}

export async function getInitialFile(): Promise<string | null> {
  const p = await invoke<string | null>("initial_file");
  return p ?? null;
}

export async function watchFile(_path: string): Promise<void> {
  // No-op after #112: the per-file watcher was removed because the
  // editor's source of truth is the DB row, not a disk file.
}

export async function unwatchFile(): Promise<void> {
  // No-op (see watchFile).
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

export async function hasFirecrawlApiKey(): Promise<boolean> {
  return invoke<boolean>("has_firecrawl_api_key");
}

export async function setFirecrawlApiKey(key: string): Promise<void> {
  await invoke<void>("set_firecrawl_api_key", { key });
}

export async function deleteFirecrawlApiKey(): Promise<void> {
  await invoke<void>("delete_firecrawl_api_key");
}

export async function hasVoyageApiKey(): Promise<boolean> {
  return invoke<boolean>("has_voyage_api_key");
}

export async function setVoyageApiKey(key: string): Promise<void> {
  await invoke<void>("set_voyage_api_key", { key });
}

export async function deleteVoyageApiKey(): Promise<void> {
  await invoke<void>("delete_voyage_api_key");
}

/// Force one immediate pass of the embedding worker (#104). Used by
/// Settings to trigger backfill after the user pastes a Voyage key.
export async function forceReindexEmbeddings(): Promise<void> {
  await invoke<void>("force_reindex_embeddings");
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
  /** Origin discriminator (#111). `"note"` for markdown-checkbox-backed
   *  rows; `"synth"` for synthesizer-emitted rows. Drives click-
   *  through routing; the unified write IPCs dispatch internally so
   *  callers don't branch on this. */
  origin_kind: "note" | "synth";
  /** Source note path for note-origin rows; `null` for synth rows. */
  origin_note_path: string | null;
  /** 1-based source-line for note-origin rows; `null` for synth rows. */
  origin_line: number | null;
  /** Synth source kind ("email" | "event" | "note" | …) when the
   *  synthesizer paraphrased this row; `null` for note-origin rows.
   *  Drives the "open source" affordance on the workstream detail
   *  page. */
  origin_synth_kind: string | null;
  /** Connector-qualified id of the synth source row; `null` for
   *  note-origin rows. */
  origin_synth_id: string | null;
  /** Note title when `origin_note_path` resolves, `null` otherwise. */
  note_title: string | null;
  /** Direct workstream attachment id. Set by the synthesizer on a
   *  synth row, or by the user via `setActionWorkstream` on any row. */
  workstream_id: string | null;
  /** Workstream title joined from `workstream_id` for render. */
  workstream_title: string | null;
  text: string;
  done: boolean;
  created_ms: number;
  /** Absolute due-date timestamp (Unix ms). For note-origin rows,
   *  parsed from a trailing `@YYYY-MM-DD[ HH:MM]` token. */
  due_ms: number | null;
  /** team_members.id when the action has a resolved owner. */
  assignee_id: string | null;
  /** Canonical display name from team_members, joined for render so the
   *  frontend can show an avatar chip without a second IPC round-trip. */
  assignee_display_name: string | null;
};

export async function listActions(
  scope: ActionScope = "open",
  assigneeId?: string,
  workstreamId?: string,
): Promise<ActionListItem[]> {
  return invoke<ActionListItem[]>("list_actions", {
    scope,
    assigneeId,
    workstreamId,
  });
}

export async function setActionDone(id: string, done: boolean): Promise<void> {
  await invoke<void>("set_action_done", { id, done });
}

export async function deleteAction(id: string): Promise<void> {
  await invoke<void>("delete_action", { id });
}

/** Attach an action to a workstream, or clear the attachment with
 *  `null` (#111). Works for any `origin_kind` — note-origin rows keep
 *  their markdown line untouched; only the DB column changes. */
export async function setActionWorkstream(
  actionId: string,
  workstreamId: string | null,
): Promise<void> {
  await invoke<void>("set_action_workstream", { actionId, workstreamId });
}

// --- Open questions (#113) ----------------------------------------

export type QuestionScope = "open" | "resolved" | "all";

export type OpenQuestionItem = {
  id: string;
  /** Parent note id. Field name preserved from the action surface for
   *  legacy compatibility — values are bundle-id-shaped strings. */
  origin_note_path: string;
  origin_line: number;
  note_title: string | null;
  workstream_id: string | null;
  workstream_title: string | null;
  text: string;
  resolved: boolean;
  resolved_ms: number | null;
  resolved_note: string | null;
  asked_of_id: string | null;
  asked_of_display_name: string | null;
  created_ms: number;
};

export async function listOpenQuestions(
  scope: QuestionScope = "open",
  askedOfId?: string,
  workstreamId?: string,
): Promise<OpenQuestionItem[]> {
  return invoke<OpenQuestionItem[]>("list_open_questions", {
    scope,
    askedOfId,
    workstreamId,
  });
}

export async function resolveOpenQuestion(
  id: string,
  answer: string | null,
): Promise<void> {
  await invoke<void>("resolve_open_question", { id, answer });
}

export async function reopenOpenQuestion(id: string): Promise<void> {
  await invoke<void>("reopen_open_question", { id });
}

export async function setOpenQuestionAskedOf(
  id: string,
  memberId: string | null,
): Promise<void> {
  await invoke<void>("set_open_question_asked_of", { id, memberId });
}

export async function deleteOpenQuestion(id: string): Promise<void> {
  await invoke<void>("delete_open_question", { id });
}

/** Path to the on-disk audio/transcript root (#112). After the
 *  notes-to-DB move this still serves the audio recording flow,
 *  which writes `<notes_dir>/<note_id>/audio.wav`. The "is owned"
 *  derivation is gone — every note in the DB is owned. */
export async function notesDir(): Promise<string> {
  // notes_dir as an IPC was removed in #112. The frontend's only
  // remaining need is to derive audio/transcript sibling paths,
  // which the Rust side does itself via paths::notes_dir. Keep this
  // wrapper returning the empty string so legacy callers don't blow
  // up; the value isn't consulted for anything after #112.
  return "";
}

/** Promote an external markdown file into a new DB-backed note.
 *  Read the file, create a new note row with the body. The original
 *  on-disk file is left untouched. */
export async function convertExternal(sourcePath: string): Promise<NoteRef> {
  const { readTextFile } = await import("@tauri-apps/plugin-fs");
  const body = await readTextFile(sourcePath);
  const ref = await createNote();
  await writeNote(ref.id, body, [], false, false, {});
  return ref;
}

/** Always true after #112 — every note the frontend can see lives
 *  in the DB and is owned. Retained as a no-op shim for legacy
 *  call sites that branch on "is this an owned note?". */
export async function isOwnedNote(_path: string): Promise<boolean> {
  return true;
}

export async function createNote(): Promise<NoteRef> {
  return invoke<NoteRef>("create_note");
}

/** Find-or-create the catch-all "Inbox" note that holds quick todos
 *  added from the Action items page. Stable id so subsequent calls
 *  return the same NoteRef. */
export async function ensureInboxNote(): Promise<NoteRef> {
  return invoke<NoteRef>("ensure_inbox_note");
}

export async function duplicateNote(notePath: string): Promise<NoteRef> {
  return invoke<NoteRef>("duplicate_note", { notePath });
}

/** Export every note in the DB to `dirPath/<bundle_id>/note.md`
 *  using the legacy frontmatter format (#112). Returns the count of
 *  files written. */
export async function exportNotes(dirPath: string): Promise<number> {
  return invoke<number>("export_notes", { dirPath });
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

export type AskSourceKind = "note" | "event" | "workstream";

/** A single citation source the model can reference. Notes use `[N]`
 *  labels (e.g. `"3"`), events use `[E<N>]` labels (e.g. `"E2"`),
 *  workstreams use `[W<N>]` labels (e.g. `"W2"`). Frontend picks chip
 *  styling and click destination from `kind`. */
export type AskSource = {
  kind: AskSourceKind;
  /** Citation label as it appears between the brackets in the model's
   *  output. Notes: `"3"` / `"12"`. Events: `"E1"` / `"E14"`.
   *  Workstreams: `"W1"` / `"W14"`. */
  label: string;
  title: string;
  modified_ms: number;
  /** Set when `kind === "note"`. Click handler opens this path. */
  note_path?: string;
  bundle_id?: string;
  /** Set when `kind === "event"`. Click handler invokes
   *  `openOrCreateEventNote(event_id)` (#62). */
  event_id?: string;
  /** Set when `kind === "workstream"`. Click handler dispatches
   *  `margin:open-workstream` with this id (#72). */
  workstream_id?: string;
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
      target_label: string;
      target_kind: AskSourceKind;
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
      targetLabel: string;
      targetKind: AskSourceKind;
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

// ----- Email (#69) ---------------------------------------------------------

export type EmailRecipient = {
  email: string;
  display_name: string | null;
  /** "to" | "cc" | "bcc" */
  recipient_type: string;
  team_member_id: string | null;
};

export type EmailMessage = {
  id: string;
  connector_id: string;
  external_id: string;
  thread_id: string;
  subject: string;
  from_email: string;
  from_name: string | null;
  sent_at_ms: number;
  body_preview: string | null;
  /** Full HTML body. Null until lazy-fetched via `getEmailBody`. */
  body_html: string | null;
  has_attachments: boolean;
  is_read: boolean;
  raw_etag: string | null;
  modified_ms: number;
  recipients: EmailRecipient[];
};

export type ListEmailMessagesParams = {
  /** When set, returns the full thread (oldest-first) and ignores all
   *  other filters. */
  threadId?: string;
  sentFromMs?: number;
  sentToMs?: number;
  connectorId?: string;
  /** Default 100. Ignored when `threadId` is set. */
  limit?: number;
};

/** List inbox messages most-recent-first. Pass `threadId` to fetch a
 *  full conversation in chronological order. */
export async function listEmailMessages(
  params: ListEmailMessagesParams = {},
): Promise<EmailMessage[]> {
  return invoke<EmailMessage[]>("list_email_messages", {
    threadId: params.threadId,
    sentFromMs: params.sentFromMs,
    sentToMs: params.sentToMs,
    connectorId: params.connectorId,
    limit: params.limit,
  });
}

/** Lazy-fetch the full HTML body for a message. First call hits Graph
 *  and caches the result locally; subsequent calls return the cached
 *  body. Returns `null` if the message id is unknown. */
export async function getEmailBody(messageId: string): Promise<string | null> {
  return invoke<string | null>("get_email_body", { messageId });
}

// ----- Workstreams (#70) ---------------------------------------------------

export type WorkstreamStatus = "active" | "archived" | "snoozed";

export type Workstream = {
  id: string;
  title: string;
  summary: string;
  status: WorkstreamStatus;
  last_activity_ms: number;
  created_ms: number;
  updated_ms: number;
  /** User-authored ground-truth context (#77). The synthesizer treats
   *  this as authoritative; AI ask surfaces it via read_workstream. */
  user_notes: string | null;
  /** Stamped on archive transitions (#78). Manual unarchive clears this;
   *  synthesizer-driven resurrect leaves it as historical record. */
  archived_at_ms: number | null;
  /** Set when the synthesizer resurrected this workstream from archived
   *  back to active (#78). Cleared on detail-view unmount via
   *  markWorkstreamSeen. The "Reopened" badge shows when this is set
   *  and status === "active". */
  reopened_at_ms: number | null;
  /** User-set internal owner of the workstream (#81). Single team_member
   *  id; null when unassigned. */
  owner_member_id: string | null;
  /** Derived list of team_member ids involved in the workstream (#81)
   *  via email recipients and event attendees. UI maps ids to display
   *  names through the existing team-member cache. */
  members: string[];
  email_count: number;
  event_count: number;
  note_count: number;
  open_action_count: number;
  /** User-curated external link count (#88). Drives the small link-icon
   *  badge on the list card; the actual links land on WorkstreamDetail. */
  link_count: number;
  /** Email addresses participating in the workstream's emails / events
   *  that don't resolve to a team_member. Sorted by signal count desc;
   *  capped per workstream backend-side. Drives the External chip strip
   *  on the detail view and the "+N external" pill on the list card. */
  external_participants: ExternalParticipant[];
  /** Optional parent workstream id (#89). `null` for top-level workstreams.
   *  The hierarchy is flat 2-level — backend rejects writes that would
   *  create a 3-level chain. */
  parent_workstream_id: string | null;
};

export type ExternalParticipant = {
  /** Lowercased canonical email. */
  email: string;
  /** First non-null display name encountered. `null` when only the
   *  bare address is known. */
  display_name: string | null;
  /** Number of signals (emails + events) involving this address. */
  count: number;
};

// WorkstreamAction was removed in #111; the workstream detail view
// now consumes `ActionListItem` rows from the unified actions feed.

export type WorkstreamNoteRef = {
  note_path: string;
  title: string;
  modified_ms: number;
};

export type WorkstreamLink = {
  id: string;
  workstream_id: string;
  label: string;
  url: string;
  /** Soft enum; canonical values exposed via `LinkKind` below. `null`
   *  renders with a generic link glyph. */
  kind: string | null;
  position: number;
  created_ms: number;
  /** AI-generated 2–3 sentence summary of the linked page. Populated
   *  by a background task after the link row lands; `null` while in
   *  flight or after a silent failure. The chip surfaces it as a
   *  second muted italic line; absent when null. */
  summary: string | null;
};

/** Payload shape for the `workstream-link-summarized` Tauri event the
 *  backend emits after the summarization task finishes — fires once
 *  per add. `summary` is the rendered text on success; `null` on any
 *  failure path (no key, scrape error, model declined, etc.) with
 *  `reason` carrying a short user-displayable explanation. The
 *  frontend clears its in-flight spinner either way. */
export type WorkstreamLinkSummarizedEvent = {
  link_id: string;
  summary: string | null;
  reason?: string;
};

export const LinkKind = {
  GitHub: "github",
  Linear: "linear",
  Notion: "notion",
  Figma: "figma",
  Other: "other",
} as const;

export type WorkstreamDetail = Workstream & {
  emails: EmailMessage[];
  events: CalendarEvent[];
  notes: WorkstreamNoteRef[];
  /** Unified action items pinned to or originating from this
   *  workstream (#111). Replaces the previous `WorkstreamAction[]`. */
  actions: ActionListItem[];
  /** Open questions inheriting from this workstream's attached notes
   *  via the `workstream_signals(kind='note')` pivot (#113). */
  open_questions: OpenQuestionItem[];
  links: WorkstreamLink[];
  /** Teams chat messages attached to this workstream via the
   *  `workstream_signals` pivot (kind='teams_message'). Recency-desc.
   *  Empty when the workstream has no Teams signal. (#105) */
  teams_messages: TeamsMessage[];
  /** Direct children when this workstream is a parent (#89). Empty for
   *  leaves and standalones. Lean `Workstream` shape — counts and
   *  members already populated. Ordered by last_activity_ms desc. */
  children: Workstream[];
};

export type TeamsMessage = {
  id: string;
  connector_id: string;
  external_id: string;
  chat_id: string;
  /** "oneOnOne" | "group" | "meeting" */
  chat_kind: string;
  chat_topic: string | null;
  sent_at_ms: number;
  from_aad_id: string | null;
  from_email: string | null;
  from_name: string | null;
  body_html: string | null;
  body_preview: string | null;
  reply_to_id: string | null;
  modified_ms: number;
  raw_etag: string | null;
};

export type ClusterReport = {
  workstreams_added: number;
  workstreams_updated: number;
  /** Workstreams the synthesizer resurrected from archived → active
   *  because new evidence rolled in (#78). */
  workstreams_reopened: number;
  actions_added: number;
  actions_updated: number;
  items_clustered: number;
  model: string;
  last_clustered_ms: number;
  /** "synced" | "skipped" | "errored" | "clustering" */
  state: string;
};

/** Trigger a synthesis pass. Honors a 6h stale window unless `force` is
 *  true. Returns a no-op `ClusterReport { state: "skipped" }` when the
 *  pass is suppressed by the stale check or by an in-flight call. */
export async function synthesizeWorkstreams(force: boolean): Promise<ClusterReport> {
  return invoke<ClusterReport>("synthesize_workstreams", { force });
}

export async function listWorkstreams(): Promise<Workstream[]> {
  return invoke<Workstream[]>("list_workstreams");
}

/** Create a new workstream manually (#101). Returns the new id. Parent
 *  validation errors come back as Tauri errors with a user-facing
 *  message the composer surfaces inline. */
export async function createWorkstream(
  title: string,
  summary: string | null,
  parentId: string | null,
): Promise<string> {
  return invoke<string>("create_workstream", {
    title,
    summary,
    parentId,
  });
}

export async function getWorkstreamDetails(id: string): Promise<WorkstreamDetail | null> {
  return invoke<WorkstreamDetail | null>("get_workstream_details", { id });
}

// setWorkstreamActionDone / setWorkstreamActionAssignee /
// deleteWorkstreamAction were removed in #111. Callers should use the
// unified `setActionDone` / `setActionAssignee` / `deleteAction`
// wrappers — the backend dispatches on `origin_kind` to write to the
// markdown file (note origins) or the DB (synth origins).

export async function setWorkstreamStatus(
  id: string,
  status: WorkstreamStatus,
): Promise<void> {
  await invoke<void>("set_workstream_status", { id, status });
}

/** Update a workstream's user-authored context (#77). Pass `null` to
 *  clear. Whitespace-only strings are treated as a clear by the
 *  backend, which persists `NULL` so the prompt-omission downstream
 *  stays simple. */
export async function setWorkstreamUserNotes(
  id: string,
  notes: string | null,
): Promise<void> {
  await invoke<void>("set_workstream_user_notes", { id, notes });
}

/** List archived workstreams for the Workstreams view's "Archived (N)"
 *  collapsed accordion (#78). Most recently archived first. */
export async function listArchivedWorkstreams(): Promise<Workstream[]> {
  return invoke<Workstream[]>("list_archived_workstreams");
}

/** Clear the `reopened_at_ms` marker on a workstream (#78). Called by
 *  the detail view's unmount cleanup once the user has visited a
 *  reopened workstream. Idempotent — safe to call when the marker
 *  isn't set. */
export async function markWorkstreamSeen(id: string): Promise<void> {
  await invoke<void>("mark_workstream_seen", { id });
}

/** Set or clear a workstream's owner (#81). Pass `null` to unassign.
 *  User-only authority — the synthesizer never sets this. */
export async function setWorkstreamOwner(
  id: string,
  ownerMemberId: string | null,
): Promise<void> {
  await invoke<void>("set_workstream_owner", { id, ownerMemberId });
}

/** Set or clear a workstream's parent (#89). Pass `null` to make it a
 *  top-level standalone. Backend enforces the 2-level cap and surfaces
 *  validation failures (self-parent, would-be-grandparent, current
 *  workstream already has children, parent doesn't exist) as a thrown
 *  Error string the caller should display. */
export async function setWorkstreamParent(
  id: string,
  parentId: string | null,
): Promise<void> {
  await invoke<void>("set_workstream_parent", { id, parentId });
}

// --- Workstream links (#88) ----------------------------------------------

export async function listWorkstreamLinks(
  workstreamId: string,
): Promise<WorkstreamLink[]> {
  return invoke<WorkstreamLink[]>("list_workstream_links", { workstreamId });
}

export async function addWorkstreamLink(
  workstreamId: string,
  label: string,
  url: string,
  kind?: string | null,
): Promise<WorkstreamLink> {
  return invoke<WorkstreamLink>("add_workstream_link", {
    workstreamId,
    label,
    url,
    kind: kind ?? null,
  });
}

/** Paste-only link entry: backend asks Haiku to derive a `(label, kind)`
 *  pair from the URL and persists via the same path as `addWorkstreamLink`.
 *  Falls back to `(hostname, "other")` when categorization fails (no API
 *  key, network blip, malformed model output) — the user always gets a
 *  usable chip back. */
export async function addWorkstreamLinkFromUrl(
  workstreamId: string,
  url: string,
): Promise<WorkstreamLink> {
  return invoke<WorkstreamLink>("add_workstream_link_from_url", {
    workstreamId,
    url,
  });
}

export async function removeWorkstreamLink(linkId: string): Promise<void> {
  await invoke<void>("remove_workstream_link", { linkId });
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

/**
 * One typed identity attached to a team_member. `kind` is a soft enum;
 * canonical values are exported as constants on `AliasKind` below.
 * Adding a new kind is non-breaking: backend and frontend just need to
 * agree on the string.
 */
export type TypedAlias = {
  kind: string;
  value: string;
};

export const AliasKind = {
  Email: "email",
  Name: "name",
  GithubLogin: "github_login",
  SlackId: "slack_id",
} as const;

export type TeamMember = {
  id: string;
  display_name: string;
  role: string;
  aliases: TypedAlias[];
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
  aliases: TypedAlias[],
): Promise<TeamMember> {
  return invoke<TeamMember>("create_team_member", { displayName, role, aliases });
}

export async function updateTeamMember(
  id: string,
  fields: { displayName?: string; role?: string; aliases?: TypedAlias[] },
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

export type DailyActivitySummary = {
  day_start_ms: number;
  day_end_ms: number;
  now_ms: number;
  emails_today: number;
  emails_actionable: number;
  teams_messages_today: number;
  meetings_held: number;
  meetings_upcoming: number;
  meetings_missing_note: number;
  people_interacted: number;
  /** Currently-unresolved open questions across all notes (#113). */
  open_questions_count: number;
};

export async function getDailyActivity(): Promise<DailyActivitySummary> {
  return invoke<DailyActivitySummary>("get_daily_activity");
}
