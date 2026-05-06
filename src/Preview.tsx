import { useMemo } from "react";
import { render } from "./markdown";

type Props = {
  source: string;
  theme: "light" | "dark";
  /** When provided, clicking a `[ ]` / `[x]` checkbox toggles the
   *  underlying source line. Omit to keep checkboxes display-only. */
  onSourceChange?: (next: string) => void;
};

export function Preview({ source, theme, onSourceChange }: Props) {
  const html = useMemo(() => render(source), [source]);

  const onClickCapture = (e: React.MouseEvent<HTMLElement>) => {
    if (!onSourceChange) return;
    const target = e.target as HTMLElement;
    if (
      target.tagName !== "INPUT" ||
      !target.classList.contains("task-list-item-checkbox")
    ) {
      return;
    }
    // Find this checkbox's index among all task-list checkboxes in the
    // article (document order) → maps directly to the Nth task line in
    // the source, since markdown-it-task-lists emits one checkbox per
    // task line in the same order.
    const root = e.currentTarget;
    const all = root.querySelectorAll<HTMLInputElement>(
      ".task-list-item-checkbox",
    );
    const idx = Array.from(all).indexOf(target as HTMLInputElement);
    if (idx < 0) return;
    const next = toggleNthTaskLine(source, idx);
    if (next === source) return;
    // Stop the native toggle; React's re-render will reflect the new
    // source consistently. Without this, the input briefly shows the
    // wrong state until the next paint.
    e.preventDefault();
    onSourceChange(next);
  };

  return (
    <div className="preview-scroll" onClickCapture={onClickCapture}>
      <div className="preview-eyebrow">Preview</div>
      <article
        className="markdown-body note-preview"
        data-theme={theme}
        dangerouslySetInnerHTML={{ __html: html }}
      />
    </div>
  );
}

const TASK_LINE_RE = /^(\s*(?:[-*+])\s+\[)([ xX])(\])/;

function toggleNthTaskLine(source: string, n: number): string {
  const lines = source.split("\n");
  let count = 0;
  for (let i = 0; i < lines.length; i++) {
    const m = TASK_LINE_RE.exec(lines[i]);
    if (!m) continue;
    if (count === n) {
      const next = m[2] === " " ? "x" : " ";
      const innerStart = m[1].length;
      lines[i] = lines[i].slice(0, innerStart) + next + lines[i].slice(innerStart + 1);
      return lines.join("\n");
    }
    count++;
  }
  return source;
}
