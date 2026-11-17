/**
 * Helpers for interpreting CodeRabbit (the `coderabbitai` bot) PR comments.
 *
 * CodeRabbit posts three shapes on a PR:
 *  - **inline review comments** — the actionable findings, each on a `file:line`
 *    thread with a per-thread `isResolved` state (`kind === "review_comment"`);
 *  - a per-run **review summary** — the "Actionable comments posted: N" block
 *    plus a collapsible walkthrough (`kind === "review"`);
 *  - occasional **issue / status comments** (`kind === "issue"`).
 *
 * Only the inline findings are actionable and carry a resolution state; the
 * summary and status posts are rollups that never resolve, so they linger in the
 * rail even after every finding has been addressed. The rail therefore drops
 * CodeRabbit's rollups and keeps only its *unresolved* inline findings — see
 * {@link isCodeRabbitRollup}.
 *
 * Each inline finding leads with a metadata line CodeRabbit renders as
 * `_<category>_ | _<severity>_ | _<effort>_`, e.g.
 * `_📐 Maintainability & Code Quality_ | _🟡 Minor_ | _⚡ Quick win_`.
 * The severity badge is a colored circle we surface as a dot
 * ({@link codeRabbitSeverity}), and the finding's one-line summary is the first
 * bold heading ({@link codeRabbitPreview}).
 */

/** CodeRabbit's four severity levels, ordered high → low. */
export type CodeRabbitSeverity = "critical" | "major" | "minor" | "trivial";

/**
 * Whether a comment author is the CodeRabbit bot. GitHub surfaces it as
 * `coderabbitai` (GraphQL `login`) or `coderabbitai[bot]` (the App actor);
 * match either, case-insensitively.
 */
export function isCodeRabbitAuthor(author: string | null | undefined): boolean {
  return /^coderabbitai(\[bot\])?$/i.test((author ?? "").trim());
}

/**
 * Whether a comment is a CodeRabbit **rollup** — a review summary or an
 * issue/status post, as opposed to an actionable inline finding. These have no
 * resolution state, so they'd otherwise persist in the rail forever; the rail
 * hides them and shows only the (unresolved) inline findings they summarize.
 */
export function isCodeRabbitRollup(comment: {
  author: string;
  kind: string;
}): boolean {
  return (
    isCodeRabbitAuthor(comment.author) &&
    (comment.kind === "review" || comment.kind === "issue")
  );
}

/**
 * The severity of a CodeRabbit inline finding, parsed from its leading metadata
 * badge (`… | _🟡 Minor_ | …`). Matches on the colored-circle emoji + label so a
 * body that merely mentions a severity word elsewhere can't false-positive.
 * Returns `null` when the body isn't a recognizable finding (summaries, status
 * posts, non-CodeRabbit comments) so the caller can skip the dot.
 */
export function codeRabbitSeverity(body: string): CodeRabbitSeverity | null {
  // The badge is the first line; scope the match to it.
  const firstLine = body.split("\n", 1)[0] ?? "";
  if (/🔴\s*Critical/i.test(firstLine)) return "critical";
  if (/🟠\s*Major/i.test(firstLine)) return "major";
  if (/🟡\s*Minor/i.test(firstLine)) return "minor";
  if (/🔵\s*Trivial/i.test(firstLine)) return "trivial";
  return null;
}

/** Tailwind dot class + a11y label per severity, keyed off the emoji colors. */
export const CODE_RABBIT_SEVERITY_META: Record<
  CodeRabbitSeverity,
  { dotClass: string; label: string }
> = {
  critical: { dotClass: "bg-red-500", label: "Critical" },
  major: { dotClass: "bg-orange-400", label: "Major" },
  minor: { dotClass: "bg-yellow-400", label: "Minor" },
  trivial: { dotClass: "bg-blue-400", label: "Trivial" },
};

/**
 * A short, human-readable one-liner for a CodeRabbit finding: its first **bold**
 * heading (CodeRabbit's one-line summary), with whitespace collapsed. Returns
 * `null` when there's no bold heading, so the caller falls back to the raw
 * first line. Skipping the metadata badge + markdown noise keeps the compact
 * rail preview readable.
 */
export function codeRabbitPreview(body: string): string | null {
  const bold = body.match(/\*\*(.+?)\*\*/);
  if (!bold) return null;
  const text = bold[1].replace(/\s+/g, " ").trim();
  return text.length > 0 ? text : null;
}
