// TS mirror of Rust's `parse_action_line` + `strip_trailing_due_token`
// (notes.rs:903-948). The returned `text` matches `actions.text` in the
// DB exactly so the action map can be keyed on its hash and the editor
// can correlate visible checkbox lines with their indexed rows.

export type ParsedActionLine = { text: string; done: boolean };

const ABSOLUTE_TOKEN = /^\d{4}-\d{2}-\d{2}(?:\s\d{2}:\d{2})?$/;

const RELATIVE_WEEKDAYS = new Set([
  "today",
  "tomorrow",
  "mon",
  "monday",
  "tue",
  "tuesday",
  "wed",
  "wednesday",
  "thu",
  "thursday",
  "fri",
  "friday",
  "sat",
  "saturday",
  "sun",
  "sunday",
]);

function isRelativeToken(token: string): boolean {
  return RELATIVE_WEEKDAYS.has(token.toLowerCase());
}

function isAbsoluteToken(token: string): boolean {
  return ABSOLUTE_TOKEN.test(token);
}

/**
 * Parse a markdown checkbox line. Returns `null` when the line isn't a
 * checkbox or the body is empty after stripping. Strips: indent + bullet
 * (`- ` / `* ` / `+ `) + checkbox (`[X] `) + any trailing ` @<token>`
 * (recognized as ISO date or relative form). Unrecognized `@<token>`
 * remains in the text — matches the Rust parser exactly so the produced
 * `text` collides with the indexed `actions.text` for hash lookup.
 */
export function parseActionLine(rawLine: string): ParsedActionLine | null {
  const trimmedLeft = rawLine.trimStart();
  let afterBullet: string;
  if (trimmedLeft.startsWith("- ")) afterBullet = trimmedLeft.slice(2);
  else if (trimmedLeft.startsWith("* ")) afterBullet = trimmedLeft.slice(2);
  else if (trimmedLeft.startsWith("+ ")) afterBullet = trimmedLeft.slice(2);
  else return null;

  // Need `[X] x` (4 chars + non-empty body). The middle char is the marker.
  if (
    afterBullet.length < 4 ||
    afterBullet[0] !== "[" ||
    afterBullet[2] !== "]" ||
    afterBullet[3] !== " "
  ) {
    return null;
  }
  const marker = afterBullet[1]!;
  let done: boolean;
  if (marker === " ") done = false;
  else if (marker === "x" || marker === "X") done = true;
  else return null;

  const rawText = afterBullet.slice(4).trim();
  if (rawText.length === 0) return null;

  const stripped = stripTrailingDueToken(rawText);
  if (stripped.length === 0) return null;
  return { text: stripped, done };
}

/**
 * If `s` ends with a recognized ` @<token>` (absolute or relative),
 * return `s` with the token removed. Otherwise return `s` unchanged.
 * Mirrors `strip_trailing_due_token` semantics from the Rust parser.
 */
function stripTrailingDueToken(s: string): string {
  // Find the last ` @` separator. The token is everything after it,
  // trimmed of trailing whitespace.
  const m = /\s+@(\S(?:.*\S)?)\s*$/.exec(s);
  if (!m) return s;
  const token = m[1]!;
  if (!isAbsoluteToken(token) && !isRelativeToken(token)) {
    // Unrecognized token stays in the text — same call the Rust parser
    // makes so users see and fix typos.
    return s;
  }
  return s.slice(0, m.index).trimEnd();
}
