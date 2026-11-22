import { isLocalHost, localRequest } from "@/lib/localHost";
import { listSessions, type SessionEntry } from "@/lib/privateChannel";
import { filterClosing, privateScope } from "@/lib/pendingClose";
import * as React from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import {
  useQuery,
  useQueryClient,
  type QueryClient,
} from "@tanstack/react-query";
import { Button, buttonVariants } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { Chevron } from "@/components/ui/chevron";
import { useToast } from "@/components/ui/toast";
import { api, ApiError } from "@/lib/api";
import { hostControl, HostControlError } from "@/lib/hostControl";
import { useActiveHost } from "@/lib/useActiveHost";
import { selectHost, usePairedHosts } from "@/lib/hostPairing";
import { useControlAvailable } from "@/lib/useControlAvailable";
import { LIVE_SCOPE, clearClosing, markClosing } from "@/lib/pendingClose";
import { LIVE_SESSIONS_QUERY_KEY } from "@/lib/useLiveSessions";
import {
  INITIAL_SESSION_RESTART,
  sessionRestartKey,
  type SessionRestart,
} from "@/lib/sessionRestart";
import {
  ALL_LAUNCH_TARGETS,
  launchTargetKey,
  launchTargetLabel,
  sameLaunchTarget,
} from "@/lib/types";
import type { LaunchTarget, LiveSession, UserSettings } from "@/lib/types";

/** Tooltip shown when no CLI host is attached (buttons disabled). */
export const NO_HOST_TOOLTIP =
  "Your paired host is offline — run `conversations host` on it";

/** Toast shown when the host executed the signed command. */
const DELIVERED_OK =
  "Your host accepted the signed request — the session will appear here shortly.";

/** Toast text for a failed signed host request, with the way forward. */
function hostErrorMessage(err: unknown, fallback: string): string {
  if (err instanceof HostControlError) {
    if (err.code === "not_paired") return `${err.message} (Settings → Hosts)`;
    return err.message;
  }
  return err instanceof ApiError
    ? err.message
    : ((err as Error)?.message ?? fallback);
}

/**
 * After a reopen is delivered, how long to wait for the host to spawn `claude`
 * and register the resumed live session before giving up on auto-navigating.
 * The host resumes the SAME session id, so once it appears in `/api/live` we
 * can turn the current page into its live view.
 */
const REOPEN_APPEAR_TIMEOUT_MS = 15_000;
/** Poll interval while waiting for the reopened session to register. */
const REOPEN_POLL_MS = 600;
const SETTINGS_QUERY_KEY = ["user-settings"] as const;

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

// A host never starts a bare shell on a remote request, so it is not offered.
const otherTargets = (target: LaunchTarget): LaunchTarget[] =>
  ALL_LAUNCH_TARGETS.filter(
    (t) => t.agent !== "zsh" && !sameLaunchTarget(t, target),
  );

/**
 * The primary button starts the saved default agent and launcher.
 */
function defaultSettingsQuery() {
  const local = isLocalHost();
  return {
    queryKey: local ? (["local-user-settings"] as const) : SETTINGS_QUERY_KEY,
    queryFn: () =>
      local
        ? localRequest<UserSettings>("/api/local/preferences")
        : api.getUserSettings(),
    staleTime: 30_000,
  };
}

function useDefaultTarget(): LaunchTarget {
  const { data } = useQuery(defaultSettingsQuery());
  // The launcher is the host's own default: leave it unset and the host chooses.
  return { agent: data?.default_agent ?? "claude" };
}

/** Wait on the host that accepted the launch, then prime its active-session cache.
 * Central live sessions are used only when no private host is selected. */
async function waitForLive(
  queryClient: QueryClient,
  hostId: string | null,
  match: (id: string) => boolean,
): Promise<string | undefined> {
  const deadline = Date.now() + REOPEN_APPEAR_TIMEOUT_MS;
  const expired = Symbol("readiness deadline");
  async function withinDeadline<T>(
    read: Promise<T>,
  ): Promise<T | typeof expired> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      return await Promise.race([
        read,
        new Promise<typeof expired>((resolve) => {
          timer = setTimeout(
            () => resolve(expired),
            Math.max(0, deadline - Date.now()),
          );
        }),
      ]);
    } finally {
      clearTimeout(timer);
    }
  }
  while (Date.now() < deadline) {
    try {
      if (hostId) {
        const result = await withinDeadline(
          hostControl.privateRead(hostId, (channel) =>
            listSessions(channel, { runningOnly: true }),
          ),
        );
        if (result === expired) return undefined;
        const sessions = filterClosing(
          privateScope(hostId),
          result.filter((session) => session.running),
        );
        const found = sessions.find((session) => match(session.id));
        if (found) {
          const key = ["private", hostId, "active-sessions"] as const;
          await queryClient.cancelQueries({ queryKey: key, exact: true });
          queryClient.setQueryData(key, sessions);
          const archiveKey = ["private", hostId, "sessions"] as const;
          await queryClient.cancelQueries({
            queryKey: archiveKey,
            exact: true,
          });
          queryClient.setQueryData<SessionEntry[]>(
            archiveKey,
            (previous = []) => [
              ...previous.filter(
                (session) =>
                  !sessions.some((active) => active.id === session.id),
              ),
              ...sessions,
            ],
          );
          void queryClient.invalidateQueries({
            queryKey: archiveKey,
            exact: true,
          });
          return found.id;
        }
      } else {
        const sessions = await withinDeadline(
          queryClient.fetchQuery({
            queryKey: LIVE_SESSIONS_QUERY_KEY,
            queryFn: api.liveSessions,
            staleTime: 0,
          }),
        );
        if (sessions === expired) return undefined;
        const found = sessions.find((session) => match(session.id));
        if (found) return found.id;
      }
    } catch (error) {
      // The launch is acknowledged. Retry reads only; never repeat the launch.
      if (
        error instanceof HostControlError &&
        ["not_paired", "refused", "untrusted", "failed", "cancelled"].includes(
          error.code,
        )
      )
        return undefined;
    }
    await sleep(REOPEN_POLL_MS);
  }
  return undefined;
}

/** Start a session and follow the exact id acknowledged by the host. */
export function useStartNewSession() {
  const { toast } = useToast();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const activeHostId = useActiveHost();
  const [busy, setBusy] = React.useState(false);

  // Read fullscreen through a ref so the async wait below navigates with the
  // flag that's current when the new session lands — a `Ctrl-A c` from
  // fullscreen should attach to the new session still in fullscreen.
  const [searchParams] = useSearchParams();
  const fullscreenRef = React.useRef(false);
  fullscreenRef.current = searchParams.get("fullscreen") === "1";

  const start = React.useCallback(
    async (target?: LaunchTarget) => {
      if (busy) return;
      setBusy(true);
      try {
        const chosen = target ?? {
          agent:
            (await queryClient.fetchQuery(defaultSettingsQuery()))
              .default_agent ?? "claude",
        };
        const { sessionId } = await hostControl.startSession(
          chosen.agent,
          chosen.launcher,
        );
        const fresh = await waitForLive(
          queryClient,
          activeHostId,
          (id) => id === sessionId,
        );
        if (fresh) {
          const search = fullscreenRef.current ? "?fullscreen=1" : "";
          // A paired host (local-trust or remote) renders through
          // PrivateSessionView at `/private/<id>`; only the no-paired-host
          // legacy flow uses `/sessions/<id>` (central `SessionRoute`).
          navigate(
            `/${activeHostId ? "private" : "sessions"}/${encodeURIComponent(fresh)}${search}`,
            {
              state: { fitNewSession: fresh },
            },
          );
        } else {
          toast(DELIVERED_OK, "default");
        }
      } catch (err) {
        toast(hostErrorMessage(err, "Could not reach your host."), "error");
      } finally {
        setBusy(false);
      }
    },
    [busy, toast, navigate, queryClient, activeHostId],
  );

  return { start, busy };
}

/**
 * Close a live session and, when the one being closed is the current view, fall
 * back to another of the viewer's live sessions instead of stranding them on
 * the closed view. The fallback prefers the neighbour to the RIGHT in the given
 * order (the tabs render left→right), wrapping to the left only when the closed
 * session was the last one; `?fullscreen=1` is preserved so closing the active
 * terminal in fullscreen keeps you fullscreen on the next one.
 *
 * `sessions` must be in the same order the tabs render (see `LiveSessionDetail`),
 * so "to the right" matches what the user sees. Closing a non-current session
 * (e.g. another tab's ✕) just closes it — no navigation.
 *
 * The actual close action (`api.completeLive` for the author's hard end,
 * `api.dismissLive` for a sharee's soft-close) and its messages are injected so
 * the shared list/navigation/cache-invalidation logic lives in one place.
 *
 * Closing is intentionally NOT confirmed: both paths are non-destructive — the
 * session stays in the sessions view and can be re-opened as live — so a
 * confirm dialog would only add friction.
 */
function useCloseLiveSession(
  sessions: LiveSession[],
  currentId: string,
  opts: {
    action: (id: string) => Promise<unknown>;
    forbiddenMsg: string;
    defaultErr: string;
  },
) {
  const { toast } = useToast();
  const navigate = useNavigate();
  const [searchParams] = useSearchParams();
  const queryClient = useQueryClient();
  const [busyId, setBusyId] = React.useState<string | null>(null);
  const { action, forbiddenMsg, defaultErr } = opts;

  const run = React.useCallback(
    async (targetId: string) => {
      if (busyId) return;
      setBusyId(targetId);

      // Optimistic: pull the tab now so closing feels instant, and mark the id
      // closing so a poll that was already in flight cannot land it back.
      markClosing(LIVE_SCOPE, targetId);
      const previous = queryClient.getQueryData<LiveSession[]>(
        LIVE_SESSIONS_QUERY_KEY,
      );
      await queryClient.cancelQueries({ queryKey: LIVE_SESSIONS_QUERY_KEY });
      queryClient.setQueryData<LiveSession[]>(LIVE_SESSIONS_QUERY_KEY, (prev) =>
        prev?.filter((s) => s.id !== targetId),
      );
      // Closed the session we're viewing → hop to a neighbour right away using
      // the pre-close order the user saw, before the host request resolves.
      if (targetId === currentId) {
        const idx = sessions.findIndex((s) => s.id === targetId);
        const toRight = sessions.slice(idx + 1).find((s) => s.id !== targetId);
        const rest = sessions.filter((s) => s.id !== targetId);
        const next = toRight ?? rest[rest.length - 1];
        if (next) {
          const search =
            searchParams.get("fullscreen") === "1" ? "?fullscreen=1" : "";
          navigate(`/sessions/${encodeURIComponent(next.id)}${search}`);
        }
      }

      try {
        await action(targetId);
        await Promise.all([
          queryClient.invalidateQueries({ queryKey: LIVE_SESSIONS_QUERY_KEY }),
          queryClient.invalidateQueries({ queryKey: ["sessions"] }),
        ]);
      } catch (err) {
        // Close failed — restore the tab so the user can see and retry.
        if (previous)
          queryClient.setQueryData(LIVE_SESSIONS_QUERY_KEY, previous);
        const message =
          err instanceof ApiError
            ? err.status === 403
              ? forbiddenMsg
              : err.message
            : ((err as Error)?.message ?? defaultErr);
        toast(message, "error");
      } finally {
        clearClosing(LIVE_SCOPE, targetId);
        setBusyId(null);
      }
    },
    [
      busyId,
      sessions,
      currentId,
      searchParams,
      navigate,
      queryClient,
      toast,
      action,
      forbiddenMsg,
      defaultErr,
    ],
  );

  return { run, busyId };
}

/**
 * End a live session you OWN (`POST /api/live/{id}/complete`): the session
 * graduates to a stored one and the owning CLI reaps the `claude` process.
 * Returns `end(id)` plus the id currently being ended (per-button busy).
 * See {@link useCloseLiveSession} for the shared navigation/invalidation logic.
 */
export function useEndLiveSession(sessions: LiveSession[], currentId: string) {
  const { run, busyId } = useCloseLiveSession(sessions, currentId, {
    // Ending stops the process: a signed operation the host checks. Then the
    // server graduates the session (it can no longer end anything itself).
    action: async (id: string) => {
      await hostControl.endSession(id);
      return api.completeLive(id);
    },
    forbiddenMsg: "You can only end your own sessions.",
    defaultErr: "Could not end the session.",
  });
  return { end: run, endingId: busyId };
}

/**
 * Soft-close a NOT-mine live session (`POST /api/live/{id}/dismiss`): it drops
 * out of the live list and graduates to a stored session (still findable in the
 * sessions view), but the owner's running process is left alone. Returns
 * `dismiss(id)` plus the id currently being dismissed (per-button busy).
 * See {@link useCloseLiveSession} for the shared logic.
 */
export function useDismissLiveSession(
  sessions: LiveSession[],
  currentId: string,
) {
  const { run, busyId } = useCloseLiveSession(sessions, currentId, {
    action: api.dismissLive,
    forbiddenMsg: "You cannot close this session.",
    defaultErr: "Could not close the session.",
  });
  return { dismiss: run, dismissingId: busyId };
}

/**
 * "Start new session" button — asks the user's CLI host to spawn a fresh
 * `claude` session (`POST /api/live/start-new`) and opens it once the host
 * registers it. Disabled with a tooltip when no host is attached.
 * `size`/`variant` mirror the underlying Button.
 */
export function StartNewSessionButton({
  label = "Start new session",
  size = "sm",
  variant = "outline",
}: {
  label?: string;
  size?: ButtonSize;
  variant?: ButtonVariant;
}) {
  const { available } = useControlAvailable();
  const { start, busy } = useStartNewSession();
  const defaultTarget = useDefaultTarget();
  const { hosts, active } = usePairedHosts();

  return (
    <div className="flex min-w-0 items-center gap-2">
      {!isLocalHost() && hosts.length > 0 && (
        // Styled as the outline split button beside it; the native select keeps
        // keyboard and screen-reader behaviour, the shared Chevron replaces the
        // browser's arrow.
        <span className="relative inline-flex max-w-48 min-w-0">
          <select
            aria-label="Session host"
            title="Host for new sessions"
            className={cn(
              buttonVariants({ size, variant }),
              "w-full min-w-0 cursor-pointer appearance-none truncate pr-8 text-left [&>option]:bg-popover [&>option]:text-popover-foreground",
            )}
            value={active ?? ""}
            disabled={busy}
            onChange={(event) => selectHost(event.target.value)}
          >
            {hosts.map((host) => (
              <option key={host.id} value={host.id}>
                {host.label}
              </option>
            ))}
          </select>
          <Chevron className="pointer-events-none absolute top-1/2 right-2.5 -translate-y-1/2" />
        </span>
      )}
      <HostSplitButton
        label={label}
        busyLabel="Starting…"
        available={available}
        busy={busy}
        size={size}
        variant={variant}
        defaultTarget={defaultTarget}
        onRun={(target) => void start(target)}
      />
    </div>
  );
}

/**
 * Reopen a stored session as live, then turn the current page into that live
 * view. The host resumes the SAME session id, but the live stream 404s until
 * the host has spawned `claude` and registered the session — so on a delivered
 * reopen we poll the active set (priming the destination's cache) until the id
 * appears, then navigate. If it's slow to register we fall back to the
 * "it'll appear shortly" toast rather than landing on a not-found live view.
 */
function useReopenSession(sessionId: string) {
  const { toast } = useToast();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const activeHostId = useActiveHost();
  const [busy, setBusy] = React.useState(false);

  const reopen = React.useCallback(async () => {
    if (busy) return;
    setBusy(true);
    try {
      await hostControl.reopenSession(sessionId);

      const appeared = await waitForLive(
        queryClient,
        activeHostId,
        (id) => id === sessionId,
      );
      if (appeared) {
        // Same route split as `useStartNewSession`: paired hosts render
        // via `/private/<id>`; the no-paired-host flow keeps `/sessions/<id>`.
        navigate(
          `/${activeHostId ? "private" : "sessions"}/${encodeURIComponent(sessionId)}?fullscreen=1`,
        );
      } else {
        toast(DELIVERED_OK, "default");
      }
    } catch (err) {
      toast(hostErrorMessage(err, "Could not reach your host."), "error");
    } finally {
      setBusy(false);
    }
  }, [busy, sessionId, toast, navigate, queryClient, activeHostId]);

  return { reopen, busy };
}

/**
 * "Re-open as live" button for a stored session — asks the user's CLI host to
 * resume the session in its stored cwd (`POST /api/sessions/{id}/reopen`) and,
 * once the host registers it, navigates to its live view. Disabled with a
 * tooltip when no host is attached.
 */
export function ReopenSessionButton({
  sessionId,
  size = "sm",
  variant = "outline",
}: {
  sessionId: string;
  size?: ButtonSize;
  variant?: ButtonVariant;
}) {
  const { available } = useControlAvailable();
  const { reopen, busy } = useReopenSession(sessionId);

  return (
    <HostButton
      label="Re-open as live"
      busyLabel="Re-opening…"
      available={available}
      busy={busy}
      size={size}
      variant={variant}
      onRun={() => void reopen()}
    />
  );
}

/** Close and resume the same conversation without switching to a neighbouring tab. */
export function useRestartSession(sessionId: string) {
  const { available } = useControlAvailable();
  const { toast } = useToast();
  const queryClient = useQueryClient();
  const activeHostId = useActiveHost();
  const [busy, setBusy] = React.useState(false);
  const inFlight = React.useRef(false);

  async function restart() {
    const restartKey = sessionRestartKey(sessionId);
    const previous =
      queryClient.getQueryData<SessionRestart>(restartKey) ??
      INITIAL_SESSION_RESTART;
    if (inFlight.current || previous.pending) return;
    inFlight.current = true;
    setBusy(true);
    // Dispose the old streams immediately and keep the route in progress
    // instead of loading/rendering the full stored transcript during teardown.
    queryClient.setQueryData<SessionRestart>(restartKey, {
      pending: true,
      generation: previous.generation + 1,
    });
    try {
      await hostControl.endSession(sessionId);
      await api.completeLive(sessionId);
      await queryClient.cancelQueries({ queryKey: LIVE_SESSIONS_QUERY_KEY });
      // Force a fresh terminal mount; the old stream has completed permanently.
      queryClient.setQueryData<LiveSession[]>(
        LIVE_SESSIONS_QUERY_KEY,
        (sessions) => sessions?.filter((session) => session.id !== sessionId),
      );
      await hostControl.reopenSession(sessionId);
      const appeared = await waitForLive(
        queryClient,
        activeHostId,
        (id) => id === sessionId,
      );
      if (!appeared)
        toast(
          "The session has not restarted yet. Check your CLI host; you can use Re-open as live to try again.",
          "error",
        );
    } catch (err) {
      toast(hostErrorMessage(err, "Could not restart the session."), "error");
    } finally {
      queryClient.setQueryData<SessionRestart>(restartKey, {
        pending: false,
        generation: previous.generation + 1,
      });
      void queryClient.invalidateQueries({ queryKey: ["session", sessionId] });
      inFlight.current = false;
      setBusy(false);
    }
  }

  return { restart, busy, available };
}

export function RestartSessionButton({ sessionId }: { sessionId: string }) {
  const { restart, busy, available } = useRestartSession(sessionId);
  return (
    <HostButton
      label="Restart Session"
      busyLabel="Restarting…"
      available={available}
      busy={busy}
      size="sm"
      variant="outline"
      onRun={() => void restart()}
    />
  );
}

type ButtonSize = "sm" | "default" | "lg" | "icon";
type ButtonVariant =
  "default" | "secondary" | "outline" | "ghost" | "destructive";

function HostSplitButton({
  label,
  busyLabel,
  available,
  busy,
  size,
  variant,
  defaultTarget,
  onRun,
}: {
  label: string;
  busyLabel: string;
  available: boolean;
  busy: boolean;
  size: ButtonSize;
  variant: ButtonVariant;
  defaultTarget: LaunchTarget;
  onRun: (target?: LaunchTarget) => void;
}) {
  const disabled = !available || busy;
  const alternates = otherTargets(defaultTarget);
  const controls = (
    <span className="inline-flex items-stretch">
      <Button
        size={size}
        variant={variant}
        disabled={disabled}
        onClick={() => onRun()}
        className="rounded-r-none"
      >
        {busy ? busyLabel : label}
        {!available && (
          <span
            aria-hidden="true"
            className="inline-flex h-3 w-3 items-center justify-center rounded-full border border-current text-[8px] font-medium leading-none opacity-70"
          >
            ?
          </span>
        )}
      </Button>
      <DropdownMenu.Root>
        <DropdownMenu.Trigger asChild>
          <Button
            type="button"
            size={size}
            variant={variant}
            disabled={disabled}
            className="-ml-px rounded-l-none px-2"
            aria-label={`Start with ${alternates
              .map(launchTargetLabel)
              .join(" or ")}`}
          >
            <Chevron />
          </Button>
        </DropdownMenu.Trigger>
        <DropdownMenu.Portal>
          <DropdownMenu.Content
            align="end"
            sideOffset={6}
            // z-70: this menu also renders from the "New session" split button
            // inside the LiveTerminal fullscreen overlay (z-60). The content is
            // portaled to <body> and #root is not a stacking context, so a z-50
            // menu would open *behind* the overlay (you'd only see the modal
            // scroll-lock toggle the scrollbar). Sit above the overlay, below toasts.
            className="z-70 min-w-40 rounded-md border border-border bg-popover p-1 text-sm text-popover-foreground shadow-md"
          >
            {alternates.map((alt) => (
              <DropdownMenu.Item
                key={launchTargetKey(alt)}
                onSelect={() => onRun(alt)}
                className="cursor-pointer rounded-sm px-2 py-1.5 outline-hidden hover:bg-accent focus:bg-accent"
              >
                Start with {launchTargetLabel(alt)}
              </DropdownMenu.Item>
            ))}
          </DropdownMenu.Content>
        </DropdownMenu.Portal>
      </DropdownMenu.Root>
    </span>
  );

  if (!available) {
    return (
      <span
        className="inline-flex cursor-help"
        title={NO_HOST_TOOLTIP}
        aria-label={NO_HOST_TOOLTIP}
      >
        {controls}
      </span>
    );
  }
  return controls;
}

/**
 * Shared button shell for the host actions. When no host is attached the button
 * is disabled and carries a "?" marker inside it explaining why. A disabled
 * button has `pointer-events-none`, so its own tooltip never fires — instead the
 * button is wrapped in a `<span>` carrying the `title`, and hover events fall
 * through to that span, so the explanation shows over the whole button.
 */
function HostButton({
  label,
  busyLabel,
  available,
  busy,
  size,
  variant,
  onRun,
}: {
  label: string;
  busyLabel: string;
  available: boolean;
  busy: boolean;
  size: ButtonSize;
  variant: ButtonVariant;
  onRun: () => void;
}) {
  const disabled = !available || busy;
  const button = (
    <Button size={size} variant={variant} disabled={disabled} onClick={onRun}>
      {busy ? busyLabel : label}
      {!available && (
        <span
          aria-hidden="true"
          className="inline-flex h-3 w-3 items-center justify-center rounded-full border border-current text-[8px] font-medium leading-none opacity-70"
        >
          ?
        </span>
      )}
    </Button>
  );

  if (!available) {
    return (
      <span
        className="inline-flex cursor-help"
        title={NO_HOST_TOOLTIP}
        aria-label={NO_HOST_TOOLTIP}
      >
        {button}
      </span>
    );
  }
  return button;
}
