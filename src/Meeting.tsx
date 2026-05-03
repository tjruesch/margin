import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";

import { LevelMeter } from "./LevelMeter";
import {
  deleteMeetingFiles,
  hasAnthropicApiKey,
  startMeetingRecording,
  stopMeetingRecording,
  summarizeMeeting,
  transcribe,
} from "./file";
import type { AISettings, SummaryModel } from "./settingsStore";

type MeetingState =
  | { kind: "idle" }
  | { kind: "recording"; id: string; startedAt: number }
  | { kind: "transcribing"; wavPath: string; pct: number; modelDl?: { downloaded: number; total: number } }
  | { kind: "summarizing"; transcriptPath: string }
  | { kind: "error"; message: string; retryable: boolean; transcriptPath?: string };

type Props = {
  ai: AISettings;
  onMdReady: (path: string) => void;
  onExclusiveChange: (exclusive: boolean) => void;
};

const SUMMARY_LABEL: Record<SummaryModel, string> = {
  "claude-sonnet-4-6": "Claude Sonnet 4.6",
  "claude-opus-4-7": "Claude Opus 4.7",
};

export function Meeting({ ai, onMdReady, onExclusiveChange }: Props) {
  const [state, setState] = useState<MeetingState>({ kind: "idle" });
  const [title, setTitle] = useState<string>("");
  const [hasKey, setHasKey] = useState<boolean>(true);
  const [sysAvailable, setSysAvailable] = useState<boolean>(true);

  const stateRef = useRef(state);
  useEffect(() => {
    stateRef.current = state;
  }, [state]);

  // Lock mode switching while in-progress
  useEffect(() => {
    const exclusive =
      state.kind === "recording" ||
      state.kind === "transcribing" ||
      state.kind === "summarizing";
    onExclusiveChange(exclusive);
  }, [state.kind, onExclusiveChange]);

  // Hydrate API key status whenever we're at idle
  useEffect(() => {
    if (state.kind === "idle") {
      void hasAnthropicApiKey().then(setHasKey);
      setSysAvailable(true);
    }
  }, [state.kind]);

  // Listen for sysaudio-unavailable (system audio permission denied or SCK init failed)
  useEffect(() => {
    const un = listen<string>("sysaudio-unavailable", (_e) => {
      setSysAvailable(false);
    });
    return () => {
      un.then((u) => u());
    };
  }, []);

  // ----- pipeline drivers ------------------------------------------------

  const runTranscribeAndSummarize = useCallback(
    async (wavPath: string) => {
      try {
        const tr = await transcribe(wavPath);
        const transcriptPath = wavPath.replace(/\.wav$/, ".transcript.json");
        // tr is consumed via the sidecar; we just need the path going forward
        void tr;
        setState({ kind: "summarizing", transcriptPath });
        try {
          const mdPath = await summarizeMeeting(
            transcriptPath,
            title.trim() || "Untitled meeting",
            ai.summaryModel,
          );
          // Hand the file off to the editor
          onMdReady(mdPath);
          setState({ kind: "idle" });
          setTitle("");
        } catch (err) {
          setState({
            kind: "error",
            message: typeof err === "string" ? err : "Summarization failed.",
            retryable: true,
            transcriptPath,
          });
        }
      } catch (err) {
        setState({
          kind: "error",
          message: typeof err === "string" ? err : "Transcription failed.",
          retryable: true,
        });
      }
    },
    [ai.summaryModel, onMdReady, title],
  );

  // transcribe-progress + model-download-progress while in `transcribing`
  useEffect(() => {
    if (state.kind !== "transcribing") return;
    const unTr = listen<number>("transcribe-progress", (e) => {
      const pct = typeof e.payload === "number" ? e.payload : 0;
      setState((s) => (s.kind === "transcribing" ? { ...s, pct } : s));
    });
    const unDl = listen<{ downloaded: number; total: number }>(
      "model-download-progress",
      (e) => {
        if (!e.payload) return;
        setState((s) =>
          s.kind === "transcribing"
            ? { ...s, modelDl: { downloaded: e.payload.downloaded, total: e.payload.total } }
            : s,
        );
      },
    );
    return () => {
      unTr.then((u) => u());
      unDl.then((u) => u());
    };
  }, [state.kind]);

  // ----- actions ---------------------------------------------------------

  const onStart = useCallback(async () => {
    try {
      const id = await startMeetingRecording(title.trim() || "Untitled meeting", ai.recordSystemAudio);
      setState({ kind: "recording", id, startedAt: Date.now() });
    } catch (err) {
      setState({
        kind: "error",
        message: typeof err === "string" ? err : "Failed to start recording.",
        retryable: true,
      });
    }
  }, [ai.recordSystemAudio, title]);

  const onStop = useCallback(async () => {
    if (state.kind !== "recording") return;
    try {
      const wavPath = await stopMeetingRecording();
      setState({ kind: "transcribing", wavPath, pct: 0 });
      void runTranscribeAndSummarize(wavPath);
    } catch (err) {
      setState({
        kind: "error",
        message: typeof err === "string" ? err : "Failed to stop recording.",
        retryable: true,
      });
    }
  }, [state, runTranscribeAndSummarize]);

  const onDiscard = useCallback(async () => {
    if (state.kind !== "recording") return;
    const id = state.id;
    try {
      await stopMeetingRecording(); // finalize cleanly
    } catch {
      /* ignore — we're discarding anyway */
    }
    try {
      await deleteMeetingFiles(id);
    } catch (err) {
      console.warn("delete_meeting_files failed:", err);
    }
    setState({ kind: "idle" });
  }, [state]);

  const onRetry = useCallback(() => {
    setState({ kind: "idle" });
  }, []);

  // ----- render ----------------------------------------------------------

  return (
    <div className="meeting">
      {state.kind === "idle" && (
        <MeetingIdle
          title={title}
          onTitleChange={setTitle}
          ai={ai}
          hasKey={hasKey}
          onStart={onStart}
        />
      )}
      {state.kind === "recording" && (
        <MeetingRecording
          startedAt={state.startedAt}
          sysAvailable={sysAvailable}
          recordingSysAudio={ai.recordSystemAudio}
          onStop={onStop}
          onDiscard={onDiscard}
        />
      )}
      {state.kind === "transcribing" && (
        <MeetingProcessing
          title="Transcribing…"
          subtitle={
            state.modelDl
              ? `Downloading Whisper model · ${(state.modelDl.downloaded / 1e6).toFixed(1)} / ${(state.modelDl.total / 1e6).toFixed(1)} MB`
              : "Local Whisper · base.en"
          }
          progress={state.pct}
        />
      )}
      {state.kind === "summarizing" && (
        <MeetingProcessing
          title="Summarizing…"
          subtitle={`with ${SUMMARY_LABEL[ai.summaryModel]}`}
        />
      )}
      {state.kind === "error" && (
        <MeetingError
          message={state.message}
          onRetry={onRetry}
        />
      )}
    </div>
  );
}

// ---------- subcomponents -------------------------------------------------

function MeetingIdle({
  title,
  onTitleChange,
  ai,
  hasKey,
  onStart,
}: {
  title: string;
  onTitleChange: (v: string) => void;
  ai: AISettings;
  hasKey: boolean;
  onStart: () => void;
}) {
  const summary = useMemo(() => {
    const audio = ai.recordSystemAudio ? "mic + system audio" : "mic only";
    return `${audio} · ${SUMMARY_LABEL[ai.summaryModel]} summary`;
  }, [ai]);
  return (
    <div className="meeting-card">
      <h1 className="meeting-h1">New meeting</h1>
      <p className="meeting-sub">{summary}</p>
      {!hasKey && (
        <div className="banner banner-warn meeting-banner" role="alert">
          <span className="banner-msg">
            No Anthropic API key configured. Recording and transcription will work, but summarization
            will fail. Open Settings → AI to add one.
          </span>
        </div>
      )}
      <input
        className="settings-input meeting-title-input"
        placeholder="Untitled meeting"
        value={title}
        onChange={(e) => onTitleChange(e.target.value)}
        autoFocus
        onKeyDown={(e) => {
          if (e.key === "Enter") onStart();
        }}
      />
      <button className="meeting-primary" onClick={onStart}>
        Start recording
      </button>
    </div>
  );
}

function MeetingRecording({
  startedAt,
  sysAvailable,
  recordingSysAudio,
  onStop,
  onDiscard,
}: {
  startedAt: number;
  sysAvailable: boolean;
  recordingSysAudio: boolean;
  onStop: () => void;
  onDiscard: () => void;
}) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);
  const elapsedMs = now - startedAt;
  const mm = String(Math.floor(elapsedMs / 60_000)).padStart(2, "0");
  const ss = String(Math.floor((elapsedMs % 60_000) / 1000)).padStart(2, "0");

  return (
    <div className="meeting-card">
      <div className="meeting-recording-pill">
        <span className="meeting-recording-dot" /> Recording
      </div>
      <div className="meeting-timer">{mm}:{ss}</div>
      <LevelMeter />
      {recordingSysAudio && !sysAvailable && (
        <p className="meeting-warn">
          System audio unavailable — recording mic only. Grant Screen Recording permission in
          System Settings → Privacy & Security to capture system output.
        </p>
      )}
      <div className="meeting-actions">
        <button className="meeting-primary" onClick={onStop}>
          Stop & summarize
        </button>
        <button className="meeting-secondary" onClick={onDiscard}>
          Discard
        </button>
      </div>
    </div>
  );
}

function MeetingProcessing({
  title,
  subtitle,
  progress,
}: {
  title: string;
  subtitle: string;
  progress?: number;
}) {
  return (
    <div className="meeting-card">
      <div className="meeting-spinner" aria-hidden="true" />
      <h1 className="meeting-h1">{title}</h1>
      <p className="meeting-sub">{subtitle}</p>
      {typeof progress === "number" && (
        <div className="meeting-progress">
          <div
            className="meeting-progress-fill"
            style={{ width: `${Math.min(100, Math.max(0, progress)).toFixed(0)}%` }}
          />
        </div>
      )}
    </div>
  );
}

function MeetingError({
  message,
  onRetry,
}: {
  message: string;
  onRetry: () => void;
}) {
  return (
    <div className="meeting-card">
      <h1 className="meeting-h1">Something went wrong</h1>
      <p className="meeting-error">{message}</p>
      <div className="meeting-actions">
        <button className="meeting-primary" onClick={onRetry}>
          Back to start
        </button>
      </div>
    </div>
  );
}
