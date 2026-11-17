/**
 * PR URL extraction from free text.
 *
 * Matches `https://github.com/{owner}/{repo}/pull/{number}` in any text
 * block (chat messages, transcripts, etc.). De-dupes by URL so that a PR
 * mentioned multiple times in one block only appears once.
 */

export interface PrRef {
  url: string;
  owner: string;
  repo: string;
  number: number;
}

/**
 * Stable DOM id for a PR's row in the Pull Requests card, so an inline PR
 * highlight can scroll to it. Keyed on owner/repo/number (case-insensitive,
 * non-id chars → `-`) rather than the raw URL, so a highlight and its row match
 * even if the URL casing differs.
 */
export function prAnchorId(owner: string, repo: string, number: number): string {
  return `pr-anchor-${owner}-${repo}-${number}`
    .toLowerCase()
    .replace(/[^a-z0-9-]/g, "-");
}

// Match the canonical GitHub PR URL. The number must be at a word boundary
// (end of string, space, ?, # or /) so we don't grab `/files`, `/commits`,
// etc. as part of the number itself — those paths follow the number and start
// with a non-digit character, but we stop capturing at the first non-digit.
const PR_RE =
  /https:\/\/github\.com\/([a-zA-Z0-9_.-]+)\/([a-zA-Z0-9_.-]+)\/pull\/(\d+)/g;

/**
 * Extract all distinct GitHub PR URLs from `text`.
 *
 * Returns one entry per unique URL; trailing path segments like `/files` or
 * query strings are ignored (the match stops at the end of the PR number).
 */
export function extractPrUrls(text: string): PrRef[] {
  const seen = new Set<string>();
  const results: PrRef[] = [];

  let match: RegExpExecArray | null;
  // Reset lastIndex to 0 before iterating (global flag remembers state).
  PR_RE.lastIndex = 0;
  while ((match = PR_RE.exec(text)) !== null) {
    const [, owner, repo, numStr] = match;
    const number = parseInt(numStr, 10);
    // De-dupe by canonical URL (not by the full matched string, which can
    // include a trailing fragment from an adjacent character run).
    const canonical = `https://github.com/${owner}/${repo}/pull/${number}`;
    if (!seen.has(canonical)) {
      seen.add(canonical);
      results.push({ url: canonical, owner, repo, number });
    }
  }

  return results;
}

// A bare `owner/repo#number` shorthand (the form shown in the PR list and
// accepted in search alongside full URLs). Anchored to match the whole query.
const PR_SHORTHAND_RE = /^([a-zA-Z0-9_.-]+)\/([a-zA-Z0-9_.-]+)#(\d+)$/;

/**
 * Parse a search query into a single PR reference, mirroring the server's
 * `github::parse_pr_query`. Recognizes a full GitHub PR URL (with or without
 * the `https://` scheme) and the `owner/repo#N` shorthand. Returns `null` when
 * the query isn't a PR reference, so the caller falls back to text matching.
 */
export function parsePrQuery(query: string): PrRef | null {
  const q = query.trim();

  // Full URL (optionally scheme-less): reuse the canonical extractor.
  const withScheme = q.startsWith("github.com/") ? `https://${q}` : q;
  const urls = extractPrUrls(withScheme);
  if (urls.length > 0) return urls[0];

  // owner/repo#number shorthand.
  const m = PR_SHORTHAND_RE.exec(q);
  if (m) {
    const [, owner, repo, numStr] = m;
    const number = parseInt(numStr, 10);
    if (number > 0) {
      return {
        url: `https://github.com/${owner}/${repo}/pull/${number}`,
        owner,
        repo,
        number,
      };
    }
  }
  return null;
}

/** True when `ref` names the same PR as `pr` (case-insensitive owner/repo). */
export function prRefMatches(
  ref: PrRef,
  pr: { owner: string; repo: string; number: number },
): boolean {
  return (
    ref.number === pr.number &&
    ref.owner.toLowerCase() === pr.owner.toLowerCase() &&
    ref.repo.toLowerCase() === pr.repo.toLowerCase()
  );
}
