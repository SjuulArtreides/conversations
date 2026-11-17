/**
 * Strip terminal "chrome" lines from a plaintext screen mirror.
 *
 * Claude's input box draws full-width horizontal borders (box-drawing glyphs,
 * U+2500–U+257F, plus the occasional run of `_`/`-`/`=`). In the read-only
 * "Live response" plaintext pane those render as long, content-free rules that
 * just add noise. We drop any line whose visible characters are ENTIRELY such
 * border glyphs — a line that mixes a border glyph with real text (e.g.
 * `└ Tip: …`) is kept untouched.
 */

// A line is "border-only" when every non-space char is a box-drawing glyph
// (U+2500–U+257F) or a horizontal-rule character. Anchored to the whole line.
const BORDER_ONLY = /^[\s─-╿_=\-–—]+$/u;

export function stripTerminalChrome(text: string): string {
  return text
    .split("\n")
    .filter((line) => {
      const trimmed = line.trim();
      // Keep blank lines (they carry spacing) and any line with real content.
      if (trimmed === "") return true;
      return !BORDER_ONLY.test(trimmed);
    })
    .join("\n");
}
