import MarkdownIt from "markdown-it";
import anchor from "markdown-it-anchor";
import taskLists from "markdown-it-task-lists";
import footnote from "markdown-it-footnote";
import { full as emoji } from "markdown-it-emoji";
import hljs from "highlight.js";

export const md: MarkdownIt = new MarkdownIt({
  html: true,
  linkify: true,
  typographer: true,
  breaks: false,
  highlight(str: string, lang: string): string {
    if (lang && hljs.getLanguage(lang)) {
      try {
        return `<pre><code class="hljs language-${lang}">${
          hljs.highlight(str, { language: lang, ignoreIllegals: true }).value
        }</code></pre>`;
      } catch {
        /* fall through */
      }
    }
    return `<pre><code class="hljs">${md.utils.escapeHtml(str)}</code></pre>`;
  },
})
  .use(anchor, { permalink: anchor.permalink.headerLink() })
  .use(taskLists, { enabled: false, label: true })
  .use(footnote)
  .use(emoji);

export function render(source: string): string {
  return md.render(source);
}
