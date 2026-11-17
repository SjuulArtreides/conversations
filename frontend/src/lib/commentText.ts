/**
 * Clean a GitHub comment body for "move into conversation" — i.e. for injection
 * as a single prompt line into a live `claude` TUI.
 *
 * GitHub/CodeRabbit/linear bodies embed HTML: `<!-- internal state … -->` and
 * other HTML comments, `<details>`/`<summary>` wrappers, `<a>` links, `<p>`,
 * plus HTML entities. Injected raw, all of that is noise. The CLI's PTY
 * sanitizer additionally strips newlines (a `\n` would submit early), which
 * would run words together — so we flatten all whitespace to single spaces here.
 *
 * Steps: drop HTML comments → strip tags (keep their text) → decode the common
 * named/numeric entities → collapse every whitespace run to one space → trim.
 */
export function cleanCommentForConversation(body: string): string {
  return decodeEntities(
    body
      // HTML comments, incl. the large base64 `<!-- internal state … -->`.
      .replace(/<!--[\s\S]*?-->/g, " ")
      // Any remaining HTML tags — keep the visible text between them.
      .replace(/<[^>]+>/g, " "),
  )
    // Collapse all whitespace (newlines included) to single spaces.
    .replace(/\s+/g, " ")
    .trim();
}

/** Decode the handful of HTML entities common in GitHub comment bodies. */
function decodeEntities(s: string): string {
  return s
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#0*39;/g, "'")
    .replace(/&apos;/g, "'")
    .replace(/&nbsp;/g, " ")
    // Ampersand last so a literal `&amp;lt;` doesn't double-decode.
    .replace(/&amp;/g, "&");
}
