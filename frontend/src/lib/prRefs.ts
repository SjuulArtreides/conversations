/**
 * Span-aware PR-reference detection over free text.
 *
 * Where `prUrls.ts` returns a de-duped *set* of full PR URLs (for "which PRs
 * does this message mention"), this module returns the *positions* of every PR
 * reference so a renderer can wrap each occurrence inline with a "detected"
 * affordance. It also recognises the short `PR #193` / `PR 193` /
 * `pull request #193` forms, which `prUrls.ts` does not.
 *
 * Short forms carry no owner/repo, so they only resolve to a URL when a single
 * repo context is supplied (see `deriveRepoContext`) — mirroring the server's
 * single-repo rule in `detect_pr_refs` (crates/server/src/github.rs).
 */

import { extractPrUrls, type PrRef } from "./prUrls";

export type { PrRef };

/** A single repo context used to resolve short (`PR #N`) references. */
export interface RepoContext {
  owner: string;
  repo: string;
}

/** One detected PR reference together with its span in the source string. */
export interface PrRefMatch {
  /** Start index (inclusive) of the matched substring. */
  start: number;
  /** End index (exclusive) of the matched substring. */
  end: number;
  /** The exact matched substring (rendered verbatim inside the chip). */
  raw: string;
  /** The resolved PR the reference points at. */
  ref: PrRef;
}

// Full canonical PR URL, same shape as prUrls.ts but used here to capture spans.
const FULL_URL_RE =
  /https:\/\/github\.com\/([a-zA-Z0-9_.-]+)\/([a-zA-Z0-9_.-]+)\/pull\/(\d+)/g;

// Short form: `PR 193`, `PR #193`, `pull request #193` (case-insensitive). The
// `#` and the whitespace around it are optional, so a bare `PR 193` (e.g. in a
// Bash tool label) matches too. A leading word boundary keeps `compr 5` etc.
// from matching. Only emitted when a repo context is available.
const SHORT_RE = /(?:\bpull\s+request|\bpr)\s*#?\s*(\d+)\b/gi;

/**
 * Find every PR reference in `text`, with spans, ordered by position and with
 * overlaps removed (a full URL wins over a short form that falls inside it).
 *
 * Full PR URLs are always matched. Short `PR #N` forms are matched only when
 * `repoCtx` is provided, in which case they resolve to that repo.
 */
export function findPrRefs(
  text: string,
  repoCtx?: RepoContext | null,
): PrRefMatch[] {
  const matches: PrRefMatch[] = [];

  FULL_URL_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = FULL_URL_RE.exec(text)) !== null) {
    const [raw, owner, repo, numStr] = m;
    const number = parseInt(numStr, 10);
    matches.push({
      start: m.index,
      end: m.index + raw.length,
      raw,
      ref: {
        url: `https://github.com/${owner}/${repo}/pull/${number}`,
        owner,
        repo,
        number,
      },
    });
  }

  if (repoCtx) {
    SHORT_RE.lastIndex = 0;
    while ((m = SHORT_RE.exec(text)) !== null) {
      const [raw, numStr] = m;
      const number = parseInt(numStr, 10);
      if (number <= 0) continue;
      matches.push({
        start: m.index,
        end: m.index + raw.length,
        raw,
        ref: {
          url: `https://github.com/${repoCtx.owner}/${repoCtx.repo}/pull/${number}`,
          owner: repoCtx.owner,
          repo: repoCtx.repo,
          number,
        },
      });
    }
  }

  // Order by start, then drop any match that overlaps one already kept (the
  // full-URL pass runs first, so a URL is preferred over an enclosed short ref).
  matches.sort((a, b) => a.start - b.start || b.end - a.end);
  const kept: PrRefMatch[] = [];
  let lastEnd = -1;
  for (const match of matches) {
    if (match.start < lastEnd) continue;
    kept.push(match);
    lastEnd = match.end;
  }
  return kept;
}

/**
 * Resolve a single repo context from a set of full PR URLs. Returns the
 * `(owner, repo)` when every URL agrees on one repo, otherwise `null` (zero
 * URLs, or two or more distinct repos — ambiguous, so short forms are skipped).
 */
export function deriveRepoContext(
  urls: Array<Pick<PrRef, "owner" | "repo">>,
): RepoContext | null {
  let ctx: RepoContext | null = null;
  for (const { owner, repo } of urls) {
    if (!ctx) {
      ctx = { owner, repo };
    } else if (ctx.owner !== owner || ctx.repo !== repo) {
      return null;
    }
  }
  return ctx;
}

/**
 * Convenience: derive the repo context from all full PR URLs found across a
 * batch of text blocks (e.g. every message in a transcript). Short-form refs
 * are intentionally ignored here — they cannot establish a repo, only consume
 * one.
 */
export function deriveRepoContextFromTexts(texts: string[]): RepoContext | null {
  const all = texts.flatMap((t) => extractPrUrls(t));
  return deriveRepoContext(all);
}
