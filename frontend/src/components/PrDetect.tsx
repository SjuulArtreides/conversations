/**
 * In-message PR detection: a React context that lets the generic `Markdown`
 * renderer and tool-call labels highlight detected GitHub PR references inline
 * and offer to link them to the session ("Add to session details").
 *
 * The context is OPT-IN: only the session views (`SessionDetail`,
 * `LiveSessionDetail`) wrap their transcript in a `PrDetectProvider`. Everywhere
 * else `usePrDetect()` returns the disabled default, so `<Markdown>` behaves
 * exactly as before (no detection, no extra requests).
 */

import * as React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, ApiError } from "@/lib/api";
import { sessionPrsKey } from "@/components/ChatPanel";
import { findPrRefs, type PrRef, type RepoContext } from "@/lib/prRefs";
import { jumpToLinkedPr } from "@/lib/prJump";
import { cn } from "@/lib/utils";
import { useToast } from "@/components/ui/toast";

export interface PrDetectValue {
  /** When false, detection is off and renderers pass content through untouched. */
  enabled: boolean;
  /** Single repo context used to resolve short `PR #N` references (or null). */
  repoCtx: RepoContext | null;
  /** Canonical PR URLs already linked to the session (drives the "Added" state). */
  linkedUrls: Set<string>;
  /** Whether the viewer may link a PR (author / write access). */
  canAdd: boolean;
  /** Link a detected PR to the session. Rejects on failure. */
  onAdd: (ref: PrRef) => Promise<void>;
}

const DISABLED: PrDetectValue = {
  enabled: false,
  repoCtx: null,
  linkedUrls: new Set(),
  canAdd: false,
  onAdd: async () => {},
};

const PrDetectContext = React.createContext<PrDetectValue>(DISABLED);

export function usePrDetect(): PrDetectValue {
  return React.useContext(PrDetectContext);
}

export function PrDetectProvider({
  value,
  children,
}: {
  value: PrDetectValue;
  children: React.ReactNode;
}) {
  return (
    <PrDetectContext.Provider value={value}>
      {children}
    </PrDetectContext.Provider>
  );
}

/**
 * Build the `PrDetectValue` for a session: fetches the session's linked PRs
 * (reusing the `sessionPrsKey` react-query cache shared with the PR cards) and
 * wires `onAdd` to `POST /api/sessions/{id}/prs`. Works for both stored and
 * live sessions (the server authorizes a live id and the `session_prs` FK was
 * relaxed so the link persists pre-graduation).
 */
export function usePrDetectValue(opts: {
  sessionId: string;
  repoCtx: RepoContext | null;
  canAdd: boolean;
  enabled?: boolean;
}): PrDetectValue {
  const { sessionId, repoCtx, canAdd, enabled = true } = opts;
  const queryClient = useQueryClient();
  const { toast } = useToast();

  const { data: prs } = useQuery({
    queryKey: sessionPrsKey(sessionId),
    queryFn: () => api.listSessionPrs(sessionId),
    enabled: enabled && !!sessionId,
    // A live session may briefly 404 before its first message; don't spam.
    retry: false,
  });

  const linkedUrls = React.useMemo(
    () => new Set((prs ?? []).map((p) => p.url)),
    [prs],
  );

  const link = useMutation({
    mutationFn: (ref: PrRef) => api.linkSessionPr(sessionId, ref.url),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: sessionPrsKey(sessionId) });
    },
    onError: (err: Error) => {
      const msg =
        err instanceof ApiError ? err.message : (err?.message ?? "link failed");
      toast(`Could not add PR: ${msg}`, "error");
    },
  });

  const { mutateAsync } = link;
  const onAdd = React.useCallback(
    (ref: PrRef) => mutateAsync(ref).then(() => undefined),
    [mutateAsync],
  );

  return React.useMemo(
    () => ({ enabled, repoCtx, linkedUrls, canAdd, onAdd }),
    [enabled, repoCtx, linkedUrls, canAdd, onAdd],
  );
}

/**
 * The "detected PR" chip: a rounded ring around the original reference text (a
 * URL link or a `PR #193` token). On hover/focus — when the viewer may add and
 * the PR isn't already linked — it reveals an "Add to session details" button.
 * An already-linked PR shows a ✓ that doubles as a "jump to the linked PR row"
 * button; clicking the chip itself jumps too, except on the inner blue link
 * (the URL style), which keeps opening the PR on GitHub.
 */
export function DetectedPr({
  prRef,
  children,
}: {
  prRef: PrRef;
  children: React.ReactNode;
}) {
  const { enabled, linkedUrls, canAdd, onAdd } = usePrDetect();
  const [pending, setPending] = React.useState(false);
  const [justAdded, setJustAdded] = React.useState(false);

  // Defensive: never render the affordance when disabled (renderers already
  // gate on this, but keep the component safe to use standalone).
  if (!enabled) return <>{children}</>;

  const linked = justAdded || linkedUrls.has(prRef.url);
  const showAdd = canAdd && !linked;

  function jump() {
    jumpToLinkedPr(prRef.owner, prRef.repo, prRef.number);
  }

  // Clicking a linked chip jumps to its row in the Pull Requests card. The
  // full-URL style wraps a real <a> (the blue link) — leave clicks on it to the
  // browser so it still opens the PR on GitHub; only non-link clicks (the chip
  // padding, the ✓) jump.
  function handleChipClick(e: React.MouseEvent) {
    if (!linked) return;
    if ((e.target as HTMLElement).closest("a")) return;
    jump();
  }

  async function add() {
    if (pending || linked) return;
    setPending(true);
    try {
      await onAdd(prRef);
      setJustAdded(true);
    } catch {
      // Error toast is surfaced by the mutation; keep the chip interactive.
    } finally {
      setPending(false);
    }
  }

  return (
    <span
      onClick={handleChipClick}
      className={cn(
        "group/prref relative isolate inline-flex items-center rounded-md px-1 ring-1 ring-inset transition-colors",
        linked
          ? "cursor-pointer bg-emerald-500/10 ring-emerald-500/40"
          : "bg-primary/10 ring-primary/40",
      )}
      title={
        linked
          ? `PR #${prRef.number} is in this session's details — click to jump to it`
          : `Detected pull request #${prRef.number}`
      }
    >
      {children}
      {linked && (
        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            jump();
          }}
          className="ml-1 text-[0.7rem] font-semibold text-emerald-400 hover:text-emerald-300"
          aria-label={`Jump to PR #${prRef.number} in the session's pull requests`}
          title="Jump to this PR"
        >
          ✓
        </button>
      )}
      {showAdd && (
        <button
          type="button"
          onClick={add}
          disabled={pending}
          aria-label={`Add pull request #${prRef.number} to session details`}
          className={cn(
            "absolute left-1/2 top-full z-20 mt-1 -translate-x-1/2 whitespace-nowrap rounded-md",
            "bg-primary px-2 py-1 text-[0.7rem] font-medium text-primary-foreground shadow-md",
            "opacity-0 transition-opacity duration-100",
            "group-hover/prref:opacity-100 group-focus-within/prref:opacity-100 focus-visible:opacity-100",
            "disabled:opacity-60",
          )}
        >
          {pending ? "Adding…" : "+ Add to session details"}
        </button>
      )}
    </span>
  );
}

/**
 * Highlight PR references inside a plain (non-Markdown) string — used for
 * tool-call labels like a Bash `description` ("Get PR 193 review comments").
 * Splits the text on detected references and wraps each in a `DetectedPr` chip.
 */
export function DetectedInline({ text }: { text: string }) {
  const { enabled, repoCtx } = usePrDetect();
  const refs = React.useMemo(
    () => (enabled ? findPrRefs(text, repoCtx) : []),
    [enabled, text, repoCtx],
  );

  if (!refs.length) return <>{text}</>;

  const out: React.ReactNode[] = [];
  let cursor = 0;
  refs.forEach((m, i) => {
    if (m.start > cursor) out.push(text.slice(cursor, m.start));
    out.push(
      <DetectedPr key={`${m.start}-${i}`} prRef={m.ref}>
        {m.raw}
      </DetectedPr>,
    );
    cursor = m.end;
  });
  if (cursor < text.length) out.push(text.slice(cursor));
  return <>{out}</>;
}
