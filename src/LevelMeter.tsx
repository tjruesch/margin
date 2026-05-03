import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";

/**
 * Animated horizontal bar driven by the backend's `audio-level` event
 * (mic-side RMS, ~30 Hz). Smooths the input so the bar doesn't twitch.
 */
export function LevelMeter() {
  const [level, setLevel] = useState(0);
  const smoothed = useRef(0);

  useEffect(() => {
    const un = listen<number>("audio-level", (e) => {
      // RMS is ~0..0.5 for normal speech. Scale up + clamp + smooth.
      const target = Math.min(1, (e.payload ?? 0) * 2.2);
      smoothed.current = smoothed.current * 0.7 + target * 0.3;
      setLevel(smoothed.current);
    });
    return () => {
      un.then((u) => u());
    };
  }, []);

  return (
    <div className="level-meter" aria-label="Audio level">
      <div
        className="level-meter-fill"
        style={{ width: `${(level * 100).toFixed(1)}%` }}
      />
    </div>
  );
}
