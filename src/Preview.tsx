import { useMemo } from "react";
import { render } from "./markdown";

type Props = {
  source: string;
  theme: "light" | "dark";
};

export function Preview({ source, theme }: Props) {
  const html = useMemo(() => render(source), [source]);
  return (
    <div className="preview-scroll">
      <article
        className="markdown-body"
        data-theme={theme}
        dangerouslySetInnerHTML={{ __html: html }}
      />
    </div>
  );
}
