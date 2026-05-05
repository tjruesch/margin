import { useEffect, useState } from "react";
import { readFile, type Segment, type Transcript } from "./file";

type Props = {
  /** Path to the bundle's transcript.json. */
  path: string;
};

export function TranscriptView({ path }: Props) {
  const [data, setData] = useState<Transcript | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setData(null);
    setError(null);
    readFile(path)
      .then((f) => {
        if (cancelled) return;
        try {
          setData(JSON.parse(f.content) as Transcript);
        } catch (e) {
          setError(`Could not parse transcript: ${(e as Error).message}`);
        }
      })
      .catch((e) => {
        if (cancelled) return;
        setError(typeof e === "string" ? e : "Failed to read transcript");
      });
    return () => {
      cancelled = true;
    };
  }, [path]);

  if (error) {
    return <div className="transcript-status transcript-error">{error}</div>;
  }
  if (!data) {
    return <div className="transcript-status">Loading transcript…</div>;
  }
  if (data.segments.length === 0) {
    return (
      <div className="transcript-status">
        Transcript is empty — Whisper didn't return any segments for this recording.
      </div>
    );
  }

  const groups = groupBySpeaker(data.segments);
  const langLabel = data.language && data.language !== "und" ? data.language.toUpperCase() : null;
  const durationLabel = formatDuration(data.duration_ms);

  return (
    <div className="transcript-view">
      <div className="transcript-meta">
        {langLabel && <span>Detected language: {langLabel}</span>}
        <span>{durationLabel}</span>
        {data.num_speakers != null && data.num_speakers > 0 && (
          <span>
            {data.num_speakers} speaker{data.num_speakers === 1 ? "" : "s"}
          </span>
        )}
      </div>
      <div className="transcript-body">
        {groups.map((g, i) => (
          <div key={i} className="transcript-paragraph">
            {g.speaker !== null && (
              <div className="transcript-speaker">Speaker {g.speaker}</div>
            )}
            <p>{g.text}</p>
          </div>
        ))}
      </div>
    </div>
  );
}

/** ≥ this gap (ms) between segments forces a paragraph break when no
 *  speaker labels are present, so unlabeled transcripts don't render as
 *  one giant wall of text. */
const PAUSE_PARAGRAPH_MS = 2000;

function groupBySpeaker(segments: Segment[]): { speaker: number | null; text: string }[] {
  const out: { speaker: number | null; text: string }[] = [];
  let cur: number | null | undefined = undefined;
  let prevEnd = 0;
  for (const seg of segments) {
    const text = seg.text.trim();
    if (!text) continue;
    const sp = seg.speaker == null ? null : seg.speaker;
    const gap = seg.start_ms - prevEnd;
    const startNew =
      out.length === 0 ||
      sp !== cur ||
      (sp === null && gap >= PAUSE_PARAGRAPH_MS);
    if (startNew) {
      out.push({ speaker: sp, text });
      cur = sp;
    } else {
      out[out.length - 1].text += " " + text;
    }
    prevEnd = seg.end_ms;
  }
  return out;
}

function formatDuration(ms: number): string {
  const totalSec = Math.round(ms / 1000);
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}
