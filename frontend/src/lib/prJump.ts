import { prAnchorId } from "./prUrls";

// Tailwind utility classes toggled on a PR row to flash it after a jump. The
// row carries a `transition-[box-shadow,background-color]` so these fade out.
const FLASH = [
  "ring-2",
  "ring-primary",
  "ring-offset-2",
  "ring-offset-background",
  "bg-primary/5",
];

/**
 * Scroll the Pull Requests card's row for this PR into view and briefly flash
 * it. Used by inline PR highlights (`DetectedPr`) so clicking a `PR #42` chip or
 * a detected PR URL jumps to the linked row. Returns `false` (a no-op) when the
 * row isn't on the page — e.g. the PR isn't linked or the card isn't rendered.
 */
export function jumpToLinkedPr(
  owner: string,
  repo: string,
  number: number,
): boolean {
  const el = document.getElementById(prAnchorId(owner, repo, number));
  if (!el) return false;
  el.scrollIntoView({ behavior: "smooth", block: "center" });
  el.classList.add(...FLASH);
  window.setTimeout(() => el.classList.remove(...FLASH), 1200);
  return true;
}
