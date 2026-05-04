import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";

type Props = {
  /** Backend event name carrying an RMS f32 in roughly 0..0.5 for speech. */
  eventName?: string;
  ariaLabel?: string;
  /** Optional short label rendered to the left of the bar (e.g. "Mic", "Sys"). */
  label?: string;
};

/**
 * Animated horizontal bar driven by a backend RMS event (~30 Hz). The
 * `* 2.2` scale + 0.7/0.3 IIR smoothing match how the mic meter has
 * always looked, so mic and system bars read at comparable scales.
 */
export function LevelMeter({
  eventName = "audio-level",
  ariaLabel = "Audio level",
  label,
}: Props) {
  const [level, setLevel] = useState(0);
  const smoothed = useRef(0);

  useEffect(() => {
    const un = listen<number>(eventName, (e) => {
      const target = Math.min(1, (e.payload ?? 0) * 2.2);
      smoothed.current = smoothed.current * 0.7 + target * 0.3;
      setLevel(smoothed.current);
    });
    return () => {
      un.then((u) => u());
    };
  }, [eventName]);

  const bar = (
    <div className="level-meter" aria-label={ariaLabel}>
      <div
        className="level-meter-fill"
        style={{ width: `${(level * 100).toFixed(1)}%` }}
      />
    </div>
  );

  if (label === undefined) return bar;

  return (
    <div className="level-meter-row">
      <span className="level-meter-label">{label}</span>
      {bar}
    </div>
  );
}
