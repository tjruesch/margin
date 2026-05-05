import { useEffect, useState } from "react";

import { LevelMeter } from "./LevelMeter";
import type { SummaryModel } from "./settingsStore";

export type NoteRecording =
  | { kind: "none"; hasTranscript: boolean; transcriptPath?: string }
  | { kind: "recording"; startedAt: number }
  | {
      kind: "transcribing";
      /** "asr" while Whisper runs, "diar" after Whisper finishes and sherpa is identifying speakers. */
      phase: "asr" | "diar";
      pct: number;
      modelDl?: { downloaded: number; total: number };
    }
  | { kind: "ready"; transcriptPath: string }
  | { kind: "reconciling" }
  | { kind: "error"; message: string; transcriptPath?: string };

const SUMMARY_LABEL: Record<SummaryModel, string> = {
  "claude-sonnet-4-6": "Claude Sonnet 4.6",
  "claude-opus-4-7": "Claude Opus 4.7",
};

type Props = {
  state: NoteRecording;
  recordingSysAudio: boolean;
  sysAvailable: boolean;
  summaryModel: SummaryModel;
  hasKey: boolean;
  /** True once the active note's body looks like a reconciled output
   *  (presence of the `## Summary` heading). Suppresses the post-
   *  recording "Recording on file" idle banner so the user doesn't keep
   *  seeing a Generate-notes CTA after they've already generated. */
  notesGenerated: boolean;
  onStop: () => void;
  onDiscard: () => void;
  onGenerate: () => void;
  onDismissError: () => void;
};

export function RecordingBanner({
  state,
  recordingSysAudio,
  sysAvailable,
  summaryModel,
  hasKey,
  notesGenerated,
  onStop,
  onDiscard,
  onGenerate,
  onDismissError,
}: Props) {
  if (state.kind === "none") {
    // The Record entry point now lives in the note header. The banner only
    // surfaces when there's a transcript already on file — that's the
    // post-recording Generate-notes affordance, which isn't covered by
    // the header.
    if (!state.hasTranscript) return null;
    // Once the user has run Generate at least once, the reconciled body
    // is in the editor and a fresh CTA is just noise.
    if (notesGenerated) return null;
    return (
      <div className="recording-banner recording-banner-idle">
        <span className="recording-banner-msg">Recording on file</span>
        <div className="recording-banner-actions">
          <button className="ghost" onClick={onGenerate} disabled={!hasKey}>
            ✨ Generate notes
          </button>
        </div>
        {!hasKey && (
          <span className="recording-banner-warn">
            Add an Anthropic API key in Settings → AI to generate.
          </span>
        )}
      </div>
    );
  }

  if (state.kind === "recording") {
    return (
      <div className="recording-banner recording-banner-rec">
        <div className="recording-rec-pill">
          <span className="recording-dot recording-dot-pulse" />
          <span>REC</span>
          <Timer startedAt={state.startedAt} />
        </div>
        {recordingSysAudio && sysAvailable ? (
          <div className="level-meters-stack">
            <LevelMeter eventName="audio-level" ariaLabel="Microphone level" label="Mic" />
            <LevelMeter eventName="sysaudio-level" ariaLabel="System audio level" label="Sys" />
          </div>
        ) : (
          <LevelMeter />
        )}
        {recordingSysAudio && !sysAvailable && (
          <span className="recording-banner-warn">
            System audio unavailable — mic only.
          </span>
        )}
        <div className="recording-banner-actions">
          <button className="recording-stop-btn" onClick={onStop}>
            Stop
          </button>
          <button className="ghost" onClick={onDiscard}>
            Discard
          </button>
        </div>
      </div>
    );
  }

  if (state.kind === "transcribing") {
    const pct = Math.min(100, Math.max(0, state.pct));
    const showProgressBar = !!state.modelDl || state.phase === "asr";
    let msg: string;
    if (state.modelDl) {
      msg = `Downloading model · ${(state.modelDl.downloaded / 1e6).toFixed(1)} / ${(state.modelDl.total / 1e6).toFixed(1)} MB`;
    } else if (state.phase === "diar") {
      msg = "Identifying speakers…";
    } else {
      msg = `Transcribing… ${pct.toFixed(0)}%`;
    }
    return (
      <div className="recording-banner recording-banner-busy">
        <div className="recording-spinner" aria-hidden="true" />
        <span className="recording-banner-msg">{msg}</span>
        {showProgressBar && (
          <div className="recording-progress">
            <div
              className="recording-progress-fill"
              style={{ width: `${pct}%` }}
            />
          </div>
        )}
      </div>
    );
  }

  if (state.kind === "ready") {
    return (
      <div className="recording-banner recording-banner-ready">
        <span className="recording-banner-msg">
          Recording captured. Generate to merge with your notes.
        </span>
        <div className="recording-banner-actions">
          <button className="recording-primary" onClick={onGenerate} disabled={!hasKey}>
            ✨ Generate notes
          </button>
          <button className="ghost" onClick={onDiscard}>
            Discard recording
          </button>
        </div>
        {!hasKey && (
          <span className="recording-banner-warn">
            Add an Anthropic API key in Settings → AI first.
          </span>
        )}
      </div>
    );
  }

  if (state.kind === "reconciling") {
    return (
      <div className="recording-banner recording-banner-busy">
        <div className="recording-spinner" aria-hidden="true" />
        <span className="recording-banner-msg">
          Reconciling notes with {SUMMARY_LABEL[summaryModel]}…
        </span>
      </div>
    );
  }

  // error
  return (
    <div className="recording-banner recording-banner-error" role="alert">
      <span className="recording-banner-msg">{state.message}</span>
      <div className="recording-banner-actions">
        {state.transcriptPath && (
          <button className="ghost" onClick={onGenerate}>
            Retry
          </button>
        )}
        <button className="ghost" onClick={onDismissError}>
          Dismiss
        </button>
      </div>
    </div>
  );
}

function Timer({ startedAt }: { startedAt: number }) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);
  const elapsedMs = Math.max(0, now - startedAt);
  const mm = String(Math.floor(elapsedMs / 60_000)).padStart(2, "0");
  const ss = String(Math.floor((elapsedMs % 60_000) / 1000)).padStart(2, "0");
  return (
    <span className="recording-timer">
      {mm}:{ss}
    </span>
  );
}
