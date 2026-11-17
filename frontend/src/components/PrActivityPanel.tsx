import * as React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { HostControlError } from "@/lib/hostControl";
import { ApiError } from "@/lib/api";
import { useSessionPrSource } from "@/lib/sessionPrSource";
import type { PrComment } from "@/lib/types";
import { humanizeTime } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { useToast } from "@/components/ui/toast";
import { Markdown } from "@/components/Markdown";
import { cleanCommentForConversation } from "@/lib/commentText";
import {
  latestPrCommentRevision,
  mergePrComments,
  prCommentsKey,
} from "@/lib/prCommentCache";
import { groupCommentsByPr, type PrCommentGroup } from "@/lib/prGroups";
import { safeHref } from "@/lib/safeHref";

const POLL_MS = 15_000;

interface Props {
  id: string;
  viewerIsAuthor?: boolean;
  /** Source-specific action availability; the host still checks every operation. */
  canManage?: boolean;
  /**
   * Whether a live CLI is attached (the session is live). The "move into
   * conversation" and "reply to PR" affordances inject into / relay through the
   * running CLI, so they only make sense when live. On a stored (graduated)
   * session this is `false`: comments render read-only, and an empty panel is
   * hidden entirely rather than showing a "no activity yet" placeholder.
   */
  interactive?: boolean;
  /**
   * Whether a live CLI is currently *connected* (reply-stream subscriber > 0).
   * Distinct from `interactive`: a session can be live but its CLI detached, in
   * which case a relayed reply would be dropped. Reply affordances stay visible
   * but **disabled** (greyed) when this is `false`, so the user sees up-front
   * that a CLI must be connected rather than discovering it via an error after
   * sending. Defaults to `true` for back-compat with callers that don't pass it.
   */
  cliAttached?: boolean;
}

/**
 * The PR-activity panel. It leads with the **open (unresolved) inline review
 * comments** — the actionable code review, each shown with its `file:line` and
 * the diff hunk it hangs on — and renders every comment body as Markdown (no
 * longer a raw plaintext dump). Resolved review comments are hidden; issue
 * comments and review summaries fall to a secondary "Discussion" section.
 *
 * On a **live** session (`interactive`, the default) the author can move a
 * comment into the running session (a sanitized live reply) and reply back to
 * the PR (relayed to the attached CLI's `gh pr comment`). On a **stored**
 * session (`interactive={false}`) those affordances are gone — there's no CLI
 * attached — so the cached comments render read-only, and an empty panel is
 * hidden entirely. The fetch is best-effort: on failure we keep the last render
 * and show a subtle error; we never crash the page.
 */
export function PrActivityPanel({
  id,
  viewerIsAuthor,
  canManage = viewerIsAuthor ?? false,
  interactive = true,
  cliAttached = true,
}: Props) {
  const source = useSessionPrSource();
  const [expanded, setExpanded] = React.useState(false);
  const [visibleCount, setVisibleCount] = React.useState(25);
  const queryClient = useQueryClient();
  const {
    data: comments = [],
    isFetched: loaded,
    isError: initialError,
  } = useQuery({
    queryKey: source.key(prCommentsKey(id)),
    queryFn: () => source.listPrComments(id),
    enabled: expanded && !!id,
    staleTime: Infinity,
    retry: false,
  });
  const [error, setError] = React.useState(false);

  React.useEffect(() => {
    if (!id || !expanded || !loaded) return;
    let cancelled = false;

    async function poll() {
      try {
        const current =
          queryClient.getQueryData<PrComment[]>(
            source.key(prCommentsKey(id)),
          ) ?? [];
        const next = await source.listPrComments(
          id,
          latestPrCommentRevision(current),
        );
        if (cancelled) return;
        queryClient.setQueryData<PrComment[]>(
          source.key(prCommentsKey(id)),
          (previous) => (previous ? mergePrComments(previous, next) : next),
        );
        setError(false);
      } catch {
        if (cancelled) return;
        // Keep whatever we last rendered; just flag the error subtly.
        setError(true);
      }
    }

    const timer = window.setInterval(() => void poll(), POLL_MS);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [id, expanded, loaded, queryClient, source]);

  // Group every comment under the PR it belongs to (most-recently-active PR
  // first), so it's clear which repo/PR each thread came from. Resolved inline
  // review comments are dropped; the count is kept for the "all resolved" note.
  const { groups, hiddenResolved } = React.useMemo(
    () => groupCommentsByPr(comments),
    [comments],
  );

  const { visibleGroups, totalVisible } = React.useMemo(() => {
    const recent = groups
      .flatMap((group) => [...group.openReviews, ...group.discussion])
      .sort((a, b) => b.created_at.localeCompare(a.created_at));
    const selected = new Set(recent.slice(0, visibleCount));
    return {
      totalVisible: recent.length,
      visibleGroups: groups.flatMap((group) => {
        const openReviews = group.openReviews
          .filter((c) => selected.has(c))
          .reverse();
        const discussion = group.discussion
          .filter((c) => selected.has(c))
          .reverse();
        return openReviews.length || discussion.length
          ? [{ ...group, openReviews, discussion }]
          : [];
      }),
    };
  }, [groups, visibleCount]);

  // On a stored (non-interactive) session, don't clutter the page with an empty
  // "PR activity" card — only surface the panel once there's something to show.
  if (!interactive && loaded && comments.length === 0) return null;

  return (
    <div className="rounded-xl border border-border bg-card p-4">
      <div className="mb-3 flex items-center justify-between gap-2">
        <button
          type="button"
          aria-expanded={expanded}
          onClick={() => setExpanded((open) => !open)}
          className="text-sm font-semibold text-muted-foreground"
        >
          PR activity {expanded ? "▾" : "▸"}
        </button>
        {(error || initialError) && (
          <span className="text-xs text-muted-foreground">
            Couldn&rsquo;t refresh PR activity.
          </span>
        )}
      </div>

      {!expanded ? null : comments.length === 0 ? (
        <p className="text-xs text-muted-foreground">
          {loaded ? "No PR activity yet." : "Loading PR activity…"}
        </p>
      ) : groups.length === 0 ? (
        <p className="text-xs text-muted-foreground">
          {hiddenResolved > 0
            ? "All review comments are resolved. 🎉"
            : "No PR activity yet."}
        </p>
      ) : (
        <div className="space-y-6">
          {visibleGroups.map((group) => (
            <PrGroupSection
              key={group.key}
              id={id}
              group={group}
              interactive={interactive}
              canManage={canManage}
              cliAttached={cliAttached}
            />
          ))}
          {totalVisible > visibleCount && (
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => setVisibleCount((n) => n + 25)}
            >
              Show more comments
            </Button>
          )}
        </div>
      )}
    </div>
  );
}

/**
 * One PR's worth of activity: a header naming the repo/PR (`owner/repo#number`,
 * linking to GitHub), its open review comments and discussion, and — for an
 * author on a live session — a reply box scoped to *this* PR.
 */
function PrGroupSection({
  id,
  group,
  interactive,
  canManage,
  cliAttached,
}: {
  id: string;
  group: PrCommentGroup;
  interactive: boolean;
  canManage: boolean;
  cliAttached: boolean;
}) {
  return (
    <section className="space-y-3">
      <a
        href={safeHref(group.url)}
        target="_blank"
        rel="noreferrer noopener"
        className="inline-flex items-center gap-1 text-sm font-semibold text-foreground hover:text-primary hover:underline"
      >
        <span className="font-mono">
          {group.owner}/{group.repo}
          <span className="text-muted-foreground">#{group.number}</span>
        </span>
        <span aria-hidden>→</span>
      </a>

      {group.openReviews.length > 0 && (
        <div>
          <h3 className="mb-2 flex items-center gap-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
            Open review comments
            <span className="rounded-full bg-amber-500/15 px-1.5 py-0.5 text-[10px] font-medium text-amber-400">
              {group.openReviews.length}
            </span>
          </h3>
          <ul className="space-y-3">
            {group.openReviews.map((c) => (
              <ReviewCommentCard
                key={c.comment_id}
                id={id}
                comment={c}
                interactive={interactive}
                canManage={canManage}
                cliAttached={cliAttached}
              />
            ))}
          </ul>
        </div>
      )}

      {group.discussion.length > 0 && (
        <div>
          {group.openReviews.length > 0 && (
            <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
              Discussion
            </h3>
          )}
          <ul className="space-y-3">
            {group.discussion.map((c) => (
              <DiscussionRow
                key={c.comment_id}
                id={id}
                comment={c}
                interactive={interactive}
                canManage={canManage}
                cliAttached={cliAttached}
              />
            ))}
          </ul>
        </div>
      )}

      {interactive && canManage && (
        <PrReplyBox
          id={id}
          cliAttached={cliAttached}
          target={{
            owner: group.owner,
            repo: group.repo,
            number: group.number,
          }}
        />
      )}
    </section>
  );
}

/** Shared header line: author · kind badge · time · GitHub link. */
function CommentMeta({ comment }: { comment: PrComment }) {
  return (
    <div className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
      <span className="font-medium text-foreground">{comment.author}</span>
      <span className="rounded border border-border px-1.5 py-0.5 font-mono text-[10px] uppercase tracking-wide">
        {comment.kind}
      </span>
      <span title={comment.created_at}>{humanizeTime(comment.created_at)}</span>
      <a
        href={safeHref(comment.html_url)}
        target="_blank"
        rel="noreferrer noopener"
        className="text-primary hover:underline"
      >
        View on GitHub →
      </a>
    </div>
  );
}

/**
 * An open inline review comment: shows its `file:line` location and the diff
 * hunk it hangs on (collapsible), with the body rendered as Markdown.
 */
function ReviewCommentCard({
  id,
  comment,
  interactive,
  canManage,
  cliAttached,
}: {
  id: string;
  comment: PrComment;
  interactive: boolean;
  canManage: boolean;
  cliAttached: boolean;
}) {
  const [replyOpen, setReplyOpen] = React.useState(false);
  const location =
    comment.path && comment.line != null
      ? `${comment.path}:${comment.line}`
      : (comment.path ?? "");
  return (
    <li className="rounded-lg border border-amber-500/30 bg-background p-3">
      {location && (
        <div className="mb-2">
          <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-[11px] text-foreground">
            {location}
          </code>
        </div>
      )}
      <CommentMeta comment={comment} />
      {comment.diff_hunk && (
        <details className="mt-2 group">
          <summary className="cursor-pointer text-xs text-muted-foreground hover:text-foreground">
            Diff context
          </summary>
          <DiffHunk hunk={comment.diff_hunk} />
        </details>
      )}
      <div className="mt-2 text-sm text-foreground">
        <Markdown allowHtml>{comment.body}</Markdown>
      </div>
      {interactive && (
        <div className="mt-2 flex flex-col gap-2">
          <div className="flex flex-wrap items-center gap-2">
            <MoveIntoConversationButton id={id} body={comment.body} />
            {canManage &&
              reviewCommentDatabaseId(comment.comment_id) != null && (
                <ReplyToCommentButton
                  open={replyOpen}
                  disabled={!cliAttached}
                  onToggle={() => setReplyOpen((v) => !v)}
                />
              )}
          </div>
          {replyOpen && reviewCommentDatabaseId(comment.comment_id) != null && (
            <InlineReplyBox
              id={id}
              cliAttached={cliAttached}
              ariaLabel="Reply to review comment"
              placeholder="Reply in this comment's thread…"
              successMessage="Reply sent to the comment thread."
              target={{
                owner: comment.owner,
                repo: comment.repo,
                number: comment.number,
                in_reply_to: reviewCommentDatabaseId(comment.comment_id)!,
              }}
              onSent={() => setReplyOpen(false)}
            />
          )}
        </div>
      )}
    </li>
  );
}

/**
 * Extract the numeric GitHub `databaseId` from a stored review comment's
 * `comment_id`. The CLI prefixes review-comment ids as `review_comment:{id}`
 * (issue/review use other prefixes). Returns `null` for any non-review-comment
 * or unparseable id, so the caller can hide the threaded-reply affordance.
 */
function reviewCommentDatabaseId(commentId: string): number | null {
  const prefix = "review_comment:";
  if (!commentId.startsWith(prefix)) return null;
  const raw = commentId.slice(prefix.length);
  const n = Number(raw);
  return Number.isInteger(n) && n > 0 ? n : null;
}

/**
 * Toggle for an inline reply composer (review-comment thread or issue comment).
 * `disabled` greys it out when no live CLI is attached — the relay would drop a
 * reply, so we block it up front with an explanatory tooltip rather than letting
 * the user type and fail.
 */
function ReplyToCommentButton({
  open,
  disabled = false,
  onToggle,
}: {
  open: boolean;
  disabled?: boolean;
  onToggle: () => void;
}) {
  return (
    <Button
      type="button"
      size="sm"
      variant="outline"
      aria-expanded={open}
      disabled={disabled}
      title={disabled ? CLI_REQUIRED_HINT : undefined}
      onClick={onToggle}
    >
      {open ? "Cancel reply" : "Reply"}
    </Button>
  );
}

/** Shared tooltip/hint shown when a reply affordance is disabled (no CLI). */
const CLI_REQUIRED_HINT = "Connect a CLI to reply.";

/**
 * Inline reply composer used by both a review-comment thread and an issue
 * comment. When `target.in_reply_to` is set the CLI replies via
 * `gh api …/comments/{id}/replies` (threaded); when it's omitted the reply is a
 * top-level `gh pr comment` on the PR — which is exactly what a reply to an
 * issue comment is. On a delivered reply it triggers a delayed PR sync (so the
 * new comment lands in the DB) and calls `onSent` to collapse the composer.
 *
 * `cliAttached` mirrors the parent gate: the composer is only mounted behind an
 * enabled toggle, but we also disable the inputs (and short-circuit `send`) when
 * the CLI drops mid-compose, so a reply is never silently lost.
 */
function InlineReplyBox({
  id,
  target,
  cliAttached,
  ariaLabel,
  placeholder,
  successMessage,
  onSent,
}: {
  id: string;
  target: {
    owner: string;
    repo: string;
    number: number;
    in_reply_to?: number;
  };
  cliAttached: boolean;
  ariaLabel: string;
  placeholder: string;
  successMessage: string;
  onSent: () => void;
}) {
  const source = useSessionPrSource();
  const { toast } = useToast();
  const [text, setText] = React.useState("");
  const [sending, setSending] = React.useState(false);
  const syncTimerRef = React.useRef<number | null>(null);
  React.useEffect(
    () => () => {
      if (syncTimerRef.current != null)
        window.clearTimeout(syncTimerRef.current);
    },
    [],
  );

  async function send() {
    const value = text.trim();
    if (!value || sending) return;
    if (!cliAttached) {
      toast(CLI_REQUIRED_HINT, "error");
      return;
    }
    setSending(true);
    try {
      const { delivered } = await source.replyToPr(id, {
        owner: target.owner,
        repo: target.repo,
        number: target.number,
        body: value,
        // Threaded reply only when we have a thread id; otherwise top-level.
        ...(target.in_reply_to != null
          ? { in_reply_to: target.in_reply_to }
          : {}),
      });
      if (delivered) {
        setText("");
        toast(successMessage);
        if (syncTimerRef.current != null) {
          window.clearTimeout(syncTimerRef.current);
        }
        syncTimerRef.current = window.setTimeout(() => {
          void source.syncPrs(id).catch(() => {
            // Best-effort: the periodic poll still catches up on its own.
          });
        }, 2000);
        onSent();
      } else {
        toast("Interrupted — no live CLI attached.", "error");
      }
    } catch (err) {
      const message =
        err instanceof ApiError
          ? err.status === 403
            ? "You can only reply to your own sessions."
            : err.message
          : ((err as Error)?.message ?? "Could not send the reply.");
      toast(message, "error");
    } finally {
      setSending(false);
    }
  }

  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        void send();
      }}
    >
      <div className="flex items-end gap-2">
        <textarea
          aria-label={ariaLabel}
          placeholder={placeholder}
          rows={2}
          value={text}
          disabled={sending || !cliAttached}
          autoFocus
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              void send();
            }
          }}
          className="flex-1 resize-none rounded-lg border border-input bg-background px-3 py-2 text-sm text-foreground placeholder:text-muted-foreground focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50"
        />
        <Button
          type="submit"
          disabled={sending || !text.trim() || !cliAttached}
        >
          {sending ? "Sending…" : "Send"}
        </Button>
      </div>
    </form>
  );
}

/**
 * An issue comment or review summary — body rendered as Markdown.
 *
 * Issue comments additionally get an inline **Reply** for the author: a reply to
 * an issue comment is just a new top-level PR comment, so the composer omits
 * `in_reply_to` and the CLI runs `gh pr comment`. Review *summaries* (kind
 * `review`) have no thread to reply into and only offer "Move into conversation"
 * (they fall back to the group-level "Reply to PR" box).
 */
function DiscussionRow({
  id,
  comment,
  interactive,
  canManage,
  cliAttached,
}: {
  id: string;
  comment: PrComment;
  interactive: boolean;
  canManage: boolean;
  cliAttached: boolean;
}) {
  const [replyOpen, setReplyOpen] = React.useState(false);
  const canReply = interactive && canManage && comment.kind === "issue";
  return (
    <li className="rounded-lg border border-border bg-background p-3">
      <CommentMeta comment={comment} />
      <div className="mt-2 text-sm text-foreground">
        <Markdown allowHtml>{comment.body}</Markdown>
      </div>
      {interactive && (
        <div className="mt-2 flex flex-col gap-2">
          <div className="flex flex-wrap items-center gap-2">
            <MoveIntoConversationButton id={id} body={comment.body} />
            {canReply && (
              <ReplyToCommentButton
                open={replyOpen}
                disabled={!cliAttached}
                onToggle={() => setReplyOpen((v) => !v)}
              />
            )}
          </div>
          {canReply && replyOpen && (
            <InlineReplyBox
              id={id}
              cliAttached={cliAttached}
              ariaLabel="Reply to comment"
              placeholder="Reply to this comment…"
              successMessage="Reply sent to the PR."
              target={{
                owner: comment.owner,
                repo: comment.repo,
                number: comment.number,
              }}
              onSent={() => setReplyOpen(false)}
            />
          )}
        </div>
      )}
    </li>
  );
}

/** Render a unified-diff hunk with +/- / @@ line tinting. */
function DiffHunk({ hunk }: { hunk: string }) {
  const lines = hunk.replace(/\n$/, "").split("\n");
  return (
    <pre className="mt-1 overflow-x-auto rounded-md bg-muted/40 p-2 font-mono text-[11px] leading-relaxed">
      {lines.map((ln, i) => {
        const tone = ln.startsWith("@@")
          ? "text-sky-400"
          : ln.startsWith("+")
            ? "text-green-400"
            : ln.startsWith("-")
              ? "text-red-400"
              : "text-muted-foreground";
        return (
          <div key={i} className={tone}>
            {ln || " "}
          </div>
        );
      })}
    </pre>
  );
}

/** "Move into conversation" — relay the comment body as a sanitized live reply. */
function MoveIntoConversationButton({
  id,
  body,
}: {
  id: string;
  body: string;
}) {
  const source = useSessionPrSource();
  const { toast } = useToast();
  const [moving, setMoving] = React.useState(false);
  const [moved, setMoved] = React.useState(false);

  async function moveIntoConversation() {
    if (moving) return;
    setMoving(true);
    try {
      await source.sendReply(id, cleanCommentForConversation(body));
      setMoved(true);
      toast("Moved into the conversation.");
    } catch (err) {
      const message =
        err instanceof ApiError || err instanceof HostControlError
          ? err.message
          : ((err as Error)?.message ?? "Could not move the comment.");
      toast(message, "error");
    } finally {
      setMoving(false);
    }
  }

  return (
    <div className="flex items-center gap-2">
      <Button
        type="button"
        size="sm"
        variant="outline"
        disabled={moving}
        onClick={() => void moveIntoConversation()}
      >
        {moving ? "Moving…" : "Move into conversation"}
      </Button>
      {moved && <span className="text-xs text-muted-foreground">Moved ✓</span>}
    </div>
  );
}

/**
 * Author-only "Reply to PR" affordance. Uses the owner/repo/number derived from
 * the most recent comment. Only rendered when such a target exists (the panel
 * gates on it). When no CLI is attached (`!cliAttached`) the input is greyed out
 * with a "Connect a CLI to reply" hint; a delivered-but-dropped reply still
 * falls back to the "interrupted — no live CLI attached" toast.
 */
function PrReplyBox({
  id,
  target,
  cliAttached,
}: {
  id: string;
  target: { owner: string; repo: string; number: number };
  cliAttached: boolean;
}) {
  const source = useSessionPrSource();
  const { toast } = useToast();
  const [text, setText] = React.useState("");
  const [sending, setSending] = React.useState(false);
  // A reply posts via the CLI's `gh`; the new comment isn't in our DB yet. After
  // a short beat (let `gh pr comment` land on GitHub) trigger a sync so the
  // comment is fetched back — the panel's poll then surfaces it.
  const syncTimerRef = React.useRef<number | null>(null);
  React.useEffect(
    () => () => {
      if (syncTimerRef.current != null)
        window.clearTimeout(syncTimerRef.current);
    },
    [],
  );

  async function send() {
    const value = text.trim();
    if (!value || sending) return;
    if (!cliAttached) {
      toast(CLI_REQUIRED_HINT, "error");
      return;
    }
    setSending(true);
    try {
      const { delivered } = await source.replyToPr(id, {
        owner: target.owner,
        repo: target.repo,
        number: target.number,
        body: value,
      });
      setText("");
      if (delivered) {
        toast("Reply sent to the PR.");
        // Wait ~2s for the comment to post, then sync so it lands in the DB.
        if (syncTimerRef.current != null) {
          window.clearTimeout(syncTimerRef.current);
        }
        syncTimerRef.current = window.setTimeout(() => {
          void source.syncPrs(id).catch(() => {
            // Best-effort: the periodic poll still catches up on its own.
          });
        }, 2000);
      } else {
        toast("Interrupted — no live CLI attached.", "error");
      }
    } catch (err) {
      const message =
        err instanceof ApiError
          ? err.status === 403
            ? "You can only reply to your own sessions."
            : err.message
          : ((err as Error)?.message ?? "Could not send the reply.");
      toast(message, "error");
    } finally {
      setSending(false);
    }
  }

  return (
    <form
      className="mt-4 border-t border-border pt-3"
      onSubmit={(e) => {
        e.preventDefault();
        void send();
      }}
    >
      <label className="mb-2 flex items-center gap-2 text-xs font-semibold text-muted-foreground">
        Reply to PR
        {!cliAttached && (
          <span className="font-normal text-muted-foreground/80">
            — {CLI_REQUIRED_HINT}
          </span>
        )}
      </label>
      <div className="flex items-end gap-2">
        <textarea
          aria-label="Reply to PR"
          placeholder="Reply to the pull request…"
          rows={2}
          value={text}
          disabled={sending || !cliAttached}
          title={!cliAttached ? CLI_REQUIRED_HINT : undefined}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              void send();
            }
          }}
          className="flex-1 resize-none rounded-lg border border-input bg-background px-3 py-2 text-sm text-foreground placeholder:text-muted-foreground focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50"
        />
        <Button
          type="submit"
          disabled={sending || !text.trim() || !cliAttached}
        >
          {sending ? "Sending…" : "Send"}
        </Button>
      </div>
    </form>
  );
}
