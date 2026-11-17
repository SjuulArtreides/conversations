import { useEffect, useRef } from "react";
import { useParams, useSearchParams } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { useLiveSessions } from "@/lib/useLiveSessions";
import { useSessionRestart } from "@/lib/sessionRestart";
import { Spinner } from "@/components/states";
import { FullscreenPending } from "@/components/FullscreenPending";
import { SessionDetail } from "./SessionDetail";
import { LiveSessionDetail } from "./LiveSessionDetail";

/**
 * Single entry point for a session by id (`/sessions/:id`).
 *
 * A session id is stable across its whole lifecycle — a live session graduates
 * into a stored session with the SAME id — so there's one URL per session and
 * this component picks the view by asking the live-sessions poll whether the id
 * is currently streaming. That's also why a freshly re-opened or brand-new live
 * session (no stored row yet) must land here and render live rather than 404 in
 * the stored view.
 *
 * While the first poll is in flight we don't yet know which view to show, so we
 * render a brief spinner instead of flashing the stored view. Within-app
 * navigation has the poll cached already, so this is instant; only a cold deep
 * link pays the one-fetch wait. When a live session completes it drops out of
 * the active set on the next poll, so this view flips to the stored transcript
 * on its own — no manual "view full conversation" step. The stored row cached
 * before the session went live (e.g. an empty just-started handoff) is
 * refetched on that flip rather than served from cache.
 */
export function SessionRoute() {
  const { id = "" } = useParams();
  const [searchParams] = useSearchParams();
  const fullscreen = searchParams.get("fullscreen") === "1";
  const { activeIds, isLoading } = useLiveSessions();
  const restart = useSessionRestart(id);
  const queryClient = useQueryClient();
  const live = activeIds.has(id);
  const wasLive = useRef(live);
  useEffect(() => {
    if (wasLive.current && !live)
      void queryClient.invalidateQueries({ queryKey: ["session", id] });
    wasLive.current = live;
  }, [id, live, queryClient]);

  if (restart.pending)
    return fullscreen ? (
      <FullscreenPending
        status={{ kind: "connecting", label: "Restarting session…" }}
      />
    ) : (
      <Spinner label="Restarting session…" />
    );
  // Fullscreen first-paint contract: a `?fullscreen=1` deep link paints the
  // fullscreen terminal shell immediately, never the normal detail page under
  // a pending spinner. The live-sessions poll decides which detail route to
  // render, but the shell replaces its windowed spinner.
  if (isLoading)
    return fullscreen ? (
      <FullscreenPending />
    ) : (
      <Spinner label="Loading session…" />
    );
  // The same id can be resumed before React commits the missing-list entry.
  // A generation key resets the stream's complete state and the PTY ended flag
  // even when removal/re-registration are batched into a single render.
  return live ? (
    <LiveSessionDetail key={restart.generation} />
  ) : (
    <SessionDetail />
  );
}
