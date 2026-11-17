import {
  SessionLinearProvider,
  useSessionLinearSource,
} from "@/lib/sessionLinearSource";
import { SessionPrProvider, useSessionPrSource } from "@/lib/sessionPrSource";
import { privateTerminalGenerationKey } from "@/lib/privateSessionLifecycle";
import { PrivateSessionActionsMenu } from "./PrivateSessionActionsMenu";
import type { SessionEntry } from "@/lib/privateChannel";
import { PrReviewIcon } from "@/components/PrReviewIcon";
import {
  createLinkClickFilter,
  createLinkedTerminal,
} from "@/lib/linkedTerminal";
import { widthBesideScrollbar } from "@/lib/scrollbarGutter";
import { createTerminalReadingView } from "@/lib/terminalReadingView";
import "@xterm/xterm/css/xterm.css";
import * as React from "react";
import { Link, useLocation, useSearchParams } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type { Terminal, IDisposable } from "@xterm/xterm";
import { SerializeAddon } from "@xterm/addon-serialize";
import { WebglAddon } from "@xterm/addon-webgl";
import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { safeHref } from "@/lib/safeHref";
import {
  openPrivatePty,
  PrivateTerminalEnded,
  type LivePtyController,
} from "@/lib/privatePty";
import {
  createInputChannel,
  hostControl,
  HostControlError,
  unlockAgain,
  type InputChannel,
} from "@/lib/hostControl";
import { proposeResizeGrid } from "@/lib/terminalSizing";
import { usePreferredFontSize } from "@/lib/terminalFontSize";
import { createTerminalFrameQueue } from "@/lib/terminalFrameQueue";
import {
  captureTerminalViewport,
  restoreTerminalViewport,
} from "@/lib/terminalViewport";
import {
  SessionActionsMenu,
  sessionMenuItemClass,
} from "@/components/SessionActionsMenu";
import { AllHostSessionTabs } from "@/components/LiveTabs";
import { Notifications } from "@/components/Notifications";
import { SettingsGearLink } from "@/components/SettingsGearLink";
import { HomeLink } from "@/components/HomeLink";
import { MobileTerminalPaste } from "@/components/MobileTerminalPaste";
import { SessionSearchDialog } from "@/components/SessionSearchDialog";
import { TerminalDiagrams } from "@/components/TerminalDiagrams";
import { useTerminalDiagramMarkers } from "@/lib/useTerminalDiagramMarkers";
import { sessionPrsKey } from "@/components/ChatPanel";
import { sessionLinearDocsKey } from "@/components/LinearDocsCard";
import { CommunicationRail } from "@/components/CommunicationRail";
import {
  useStartNewSession,
  StartNewSessionButton,
} from "@/components/HostActions";
import { Button } from "@/components/ui/button";
import { useToast } from "@/components/ui/toast";
import { useControlAvailable } from "@/lib/useControlAvailable";
import { api } from "@/lib/api";
import { useTerminalFileDrop } from "@/lib/useTerminalFileDrop";
import { prCommentPreviewsKey } from "@/lib/prCommentCache";
import type {
  AgentKind,
  LaunchTarget,
  LiveSession,
  PrComment,
  SessionLinearDoc,
  SessionPr,
} from "@/lib/types";
import {
  ALL_LAUNCH_TARGETS,
  launchTargetKey,
  launchTargetLabel,
} from "@/lib/types";
import {
  serializeTermSelection,
  selectionColorSignatures,
  isClipboardDebugEnabled,
  resolveCodeColors,
  codeColorsFromDto,
  resolveGithubOrg,
  DEFAULT_CODE_COLORS,
  type TermLike,
  type CodeColorConfig,
} from "@/lib/termClipboard";
import {
  CODE_RABBIT_SEVERITY_META,
  codeRabbitPreview,
  codeRabbitSeverity,
  isCodeRabbitAuthor,
  isCodeRabbitRollup,
  type CodeRabbitSeverity,
} from "@/lib/coderabbit";

/**
 * Backoff between PTY-stream reconnect attempts. A server redeploy SIGTERMs the
 * pod, dropping the in-flight SSE connection — the host stays up and the session
 * is still live, so we reconnect rather than declaring the terminal dead. Short
 * enough to feel instant, long enough not to hammer a restarting server.
 */
export const PTY_RECONNECT_BACKOFF_MS = 1500;

// Largest pasted image the private channel carries in one transfer (its
// 9 MiB cap). Checked against `file.size` before reading the bytes, so an
// oversized screenshot is refused without loading it on the main thread.
export const LIVE_CLIPBOARD_MAX_BYTES = 9 * 1024 * 1024;

/** Why a pasted image did not arrive. A lost connection may follow delivery. */
function pasteFailureMessage(error: unknown): string {
  if (error instanceof HostControlError && error.code !== "transport")
    return error.message;
  return "Could not reach your host privately. The image may or may not have been pasted; check the terminal before pasting it again.";
}

/**
 * Font size (px) the terminal renders at when the source grid fits the
 * container, and the floor we shrink to when it doesn't. Fitting is done by
 * scaling the FONT (see the sizing-strategy note on {@link LiveTerminal}), so
 * these bound that scale rather than a CSS transform. A preferred font size
 * saved in Settings (`lib/terminalFontSize`) replaces the base.
 */
const BASE_FONT_PX = 13;
const MIN_FONT_PX = 6;
/**
 * Upper bound for the font when "Zoom to fit" is on: the grid may be scaled
 * UP past {@link BASE_FONT_PX} to fill a large (fullscreen) viewport. Without
 * zoom the cap stays at {@link BASE_FONT_PX} (the source-grid-accurate size).
 */
const MAX_FONT_PX = 40;
const MAX_TOUCH_FONT_PX = 28;
/** Font a zoomed touch terminal is read at, and so the font the "Fit columns
 *  and rows to screen" action sizes the grid for there (a phone's 13px grid is
 *  too dense to read). Without zoom the grid renders at no more than
 *  {@link BASE_FONT_PX}, so that is the target instead. A saved preferred
 *  font size is the target on every device. The clamp bounds live in
 *  `lib/terminalSizing`. */
const RESIZE_TARGET_FONT_PX = 16;

/** Tooltip for the viewer-only zoom toggle in the Display menu. */
const ZOOM_TO_FIT_DESCRIPTION =
  "Make the text bigger or smaller so the whole terminal fits. Columns and rows stay the same.";

const FIT_GRID_DESCRIPTION =
  "Change the terminal's columns and rows to fit this screen. The session redraws at the new size.";

const RESET_ZOOM_DESCRIPTION =
  "Draw the text at its normal size, shrinking only if the terminal is wider than the screen.";

/** How long the grid action waits for the session to redraw at the requested
 *  grid before reporting the resize as not confirmed. One possible cause: a
 *  terminal window attached on the host keeps its own size. */
export const GRID_CONFIRM_TIMEOUT_MS = 4000;

/** A grid request waiting for the session to redraw at its size. `seen`
 *  records a matching frame that beat the host's answer. */
interface GridRequest {
  cols: number;
  rows: number;
  seen: boolean;
  timer: number | null;
}

/** Gap kept below a windowed terminal fitted to the window. */
const WINDOW_FIT_MARGIN_PX = 12;

/**
 * Height a windowed terminal whose top edge is `el` can fill: the window below
 * that edge. Both scaling modes use it so "fits the screen" means the same.
 * A top scrolled above the window counts as 0, so the result is never taller
 * than the window.
 */
function windowedFitHeight(el: HTMLElement): number {
  const top = Math.max(0, el.getBoundingClientRect().top);
  return Math.max(0, layoutViewportHeight() - top - WINDOW_FIT_MARGIN_PX);
}

/**
 * OpenCode owns scrolling inside its alternate-screen TUI, so xterm forwards a
 * wheel gesture to the remote PTY instead of moving browser scrollback locally.
 * Send several copies of the wheel report in one WebSocket input frame to make
 * one remote round trip cover a useful amount of history.
 */
const OPENCODE_WHEEL_INPUT_MULTIPLIER = 3;

/**
 * Vertical drag (CSS px) that makes one synthetic wheel notch on touch. About
 * one terminal row at the phone target font, so a drag tracks the content
 * roughly 1:1.
 */
const TOUCH_SCROLL_STEP_PX = 20;
/**
 * Cap on the notches a single `touchmove` may emit. A fast flick reports a large
 * jump; each notch can become a mouse report on the session's input channel, so
 * bound the burst rather than letting one event fan out unboundedly.
 */
const TOUCH_SCROLL_MAX_NOTCHES_PER_MOVE = 8;
/**
 * Touch momentum uses the recent finger velocity, then eases it out over about
 * a second. Speeds are CSS px/ms: 2 px/ms is already a quick phone flick.
 */
const TOUCH_SCROLL_MAX_SPEED_PX_MS = 2;
const TOUCH_SCROLL_MOMENTUM_DECAY_MS = 240;
const TOUCH_SCROLL_MOMENTUM_STOP_SPEED_PX_MS = 0.04;
/** A pause before release means the user placed the content deliberately. */
const TOUCH_SCROLL_MOMENTUM_STALE_MS = 80;
/** Do not turn a resumed/backgrounded animation frame into one giant jump. */
const TOUCH_SCROLL_MOMENTUM_MAX_FRAME_GAP_MS = 100;

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/**
 * Whether the terminal is scrolled to (pinned at) the bottom — i.e. the viewport
 * top is at the maximum scroll position. We only auto-scroll to the bottom on new
 * output when this is true, so a user who has scrolled UP to read history isn't
 * yanked back down by the next frame. A missing buffer (the jsdom xterm mock, or
 * before the first write) is treated as pinned, preserving the tail-by-default
 * behavior in tests and on first paint.
 */
function isPinnedToBottom(term: Terminal): boolean {
  const buf = term.buffer?.active;
  if (!buf) return true;
  return buf.viewportY >= buf.baseY;
}

function isBrowserPasteKey(e: KeyboardEvent): boolean {
  // Browser paste supplies image bytes locally. Remote Ctrl+V makes Codex try
  // the host clipboard, which is unavailable in headless Coder sessions.
  return e.key.toLowerCase() === "v" && !e.altKey && (e.ctrlKey || e.metaKey);
}

function isBrowserRefreshKey(e: KeyboardEvent): boolean {
  // Let browser reload shortcuts win even while xterm keyboard capture is armed.
  // xterm otherwise treats F5 as a terminal function key and can swallow the
  // page refresh the viewer explicitly asked for.
  return (
    e.type === "keydown" &&
    e.key === "F5" &&
    !e.altKey &&
    !e.ctrlKey &&
    !e.metaKey
  );
}

function activeElementConsumesText(
  termTextarea?: HTMLTextAreaElement,
): boolean {
  const el = document.activeElement;
  if (!el || el === document.body) return false;
  if (termTextarea && el === termTextarea) return true;
  if (!(el instanceof HTMLElement)) return false;
  if (el.closest("[data-terminal-paste]")) return true;
  if (el.isContentEditable) return true;
  return ["INPUT", "TEXTAREA", "SELECT"].includes(el.tagName);
}

function ctrlKeySequence(e: KeyboardEvent): string | null {
  if (!e.ctrlKey || e.altKey || e.metaKey) return null;
  const key = e.key.toLowerCase();
  if (key.length === 1) {
    const code = key.charCodeAt(0);
    if (code >= 97 && code <= 122) {
      return String.fromCharCode(code - 96);
    }
    switch (key) {
      case " ":
        return "\x00";
      case "[":
        return "\x1b";
      case "\\":
        return "\x1c";
      case "]":
        return "\x1d";
      case "^":
      case "6":
        return "\x1e";
      case "_":
      case "-":
        return "\x1f";
      default:
        return null;
    }
  }
  return null;
}

function keyboardEventToTerminalSequence(e: KeyboardEvent): string | null {
  if (e.isComposing || isBrowserRefreshKey(e) || isBrowserPasteKey(e))
    return null;
  const ctrl = ctrlKeySequence(e);
  if (ctrl != null) return ctrl;
  if (e.ctrlKey || e.altKey || e.metaKey) return null;

  if (e.key.length === 1) return e.key;
  if (e.key === "Enter") return e.shiftKey ? "\x1b\r" : "\r";

  switch (e.key) {
    case "Backspace":
      return "\x7f";
    case "Tab":
      return "\t";
    case "Escape":
      return "\x1b";
    case "ArrowUp":
      return "\x1b[A";
    case "ArrowDown":
      return "\x1b[B";
    case "ArrowRight":
      return "\x1b[C";
    case "ArrowLeft":
      return "\x1b[D";
    case "Delete":
      return "\x1b[3~";
    case "Home":
      return "\x1b[H";
    case "End":
      return "\x1b[F";
    case "PageUp":
      return "\x1b[5~";
    case "PageDown":
      return "\x1b[6~";
    default:
      return null;
  }
}

interface Props {
  id: string;
  /** Agent running inside this PTY. Used for agent-specific terminal behavior. */
  agent?: AgentKind;
  /**
   * Whether this pooled terminal is the one currently visible. Inactive
   * terminals keep their xterm buffer + WebSocket alive so a tab revisit needs
   * no history replay, but they must not own keyboard capture, layout work, or a
   * scarce WebGL context.
   */
  active?: boolean;
  /** Called once when the PTY stream ends (claude exited / hub closed). */
  onEnded?: () => void;
  /** Active entries read from this private host, never remote LiveSession records. */
  privateSessions?: SessionEntry[];
  /**
   * Whether the viewer authored this session. The author may always turn on
   * keyboard capture (raw keystroke injection).
   */
  viewerIsAuthor?: boolean;
  /**
   * Whether the viewer has read+write access (a non-author sharee granted
   * write). Write is the single interactive grant — it lets the sharee both
   * reply and type directly into the terminal. Either this OR `viewerIsAuthor`
   * enables the capture toggle; everyone else gets fullscreen only. Defensive
   * default `false` for older server responses.
   */
  viewerCanWrite?: boolean;
  /**
   * Whether a CLI host is currently subscribed for this live session. Defaults
   * true for older callers/tests; the live route passes the server presence bit.
   */
  cliAttached?: boolean;
  /**
   * Follow this session privately from this paired host: the screen arrives on
   * the private channel (never the server's PTY socket), dropped files and
   * pasted images go to that host only, and nothing is cached in the browser.
   */
  privateHostId: string;
}

type Conn = "connecting" | "live" | "ended" | "error";

/**
 * Whether this is a touch device (phone / tablet). On such devices the terminal
 * gets a tap-to-type affordance and an on-screen key bar, because (a) iOS only
 * raises the soft keyboard when a textarea is focused SYNCHRONOUSLY inside a
 * user gesture — our capture toggle focuses from an effect, which iOS ignores —
 * and (b) a soft keyboard has no ESC / Tab / arrow / Ctrl keys, which `claude`
 * needs. We key off `navigator.maxTouchPoints` (the modern, reliable signal —
 * `"ontouchstart" in window` is true even under jsdom and on some desktops).
 */
function isTouchDevice(): boolean {
  return (
    typeof navigator !== "undefined" && (navigator.maxTouchPoints ?? 0) > 0
  );
}

/**
 * Height of the LAYOUT viewport — deliberately NOT `visualViewport.height`.
 *
 * A raised soft keyboard shrinks the visual viewport to a fraction of the
 * screen while the fullscreen overlay (`position: fixed`) keeps covering the
 * whole layout viewport. Sizing the source grid off the visual viewport
 * therefore asked the CLI host for a grid that only fills the sliver above the
 * keyboard, and the terminal stayed that short once the keyboard went away.
 */
function layoutViewportHeight(): number {
  if (typeof window === "undefined") return 0;
  return window.innerHeight ?? 0;
}

/**
 * On-screen keys offered while capturing on a touch device — the control keys a
 * soft keyboard can't send but `claude` relies on. Each sends its raw byte
 * sequence straight to the session (bypassing xterm's `onData`, like the
 * captured keystroke path — the ESC here just interrupts claude).
 */
const MOBILE_KEYS: ReadonlyArray<{
  label: string;
  seq: string;
  title: string;
}> = [
  { label: "Esc", seq: "\x1b", title: "Escape (interrupt / clear prompt)" },
  { label: "Tab", seq: "\t", title: "Tab (accept completion)" },
  { label: "↑", seq: "\x1b[A", title: "Up arrow (history / move up)" },
  { label: "↓", seq: "\x1b[B", title: "Down arrow (history / move down)" },
  { label: "←", seq: "\x1b[D", title: "Left arrow" },
  { label: "→", seq: "\x1b[C", title: "Right arrow" },
  { label: "⏎", seq: "\r", title: "Enter (submit)" },
  { label: "^C", seq: "\x03", title: "Ctrl-C (cancel)" },
];

/**
 * Read-only-by-default xterm.js terminal mirroring the running `claude`
 * session's PTY output in real time.
 *
 * Sizing strategy
 * ---------------
 * The SOURCE PTY dictates the terminal grid (cols × rows) — each frame carries
 * the source's current dimensions and we `term.resize()` to match BEFORE
 * writing, so ANSI cursor-addressing lands on the right cell. Rendering at the
 * source grid would overflow a narrow container, so we fit to width by scaling
 * the FONT SIZE down (between {@link BASE_FONT_PX} and {@link MIN_FONT_PX}).
 *
 * We deliberately do NOT CSS-`scale()` the element: xterm maps a pointer to a
 * cell using the canvas's intrinsic (unscaled) cell size, so a CSS transform
 * desyncs hit-testing from what's drawn — text selection lands on the row above
 * the cursor, worsening toward the bottom of the screen. Font-scaling keeps the
 * rendered geometry identical to what xterm hit-tests against, so selection is
 * accurate, while still showing the whole grid instead of clipping it.
 *
 * Interaction
 * -----------
 * A toolbar offers a fullscreen toggle (fixed viewport overlay) and, for the
 * author or a sharee granted write access, a "Keyboard captured" status that
 * releases capture on click (clicking the terminal is what arms it).
 * When the host (author) enters fullscreen, capture is auto-armed — going
 * fullscreen signals intent to drive the session. While capturing, xterm
 * accepts stdin and each keystroke is base64-encoded and sent over the live-PTY
 * WebSocket (`sendInput`). ESC is FORWARDED to the session (claude uses it to
 * interrupt / clear the prompt) — capture is stopped via the toolbar toggle,
 * not the keyboard. Multi-char escape sequences such as arrow keys (`\x1b[A`)
 * are forwarded too. Shift+Enter is rewritten to Meta+Enter (`\x1b\r`) —
 * claude's newline chord — so it inserts a newline instead of submitting
 * (xterm emits a bare CR otherwise).
 *
 * A viewer with write access can also SCROLL the session without arming
 * capture: OpenCode owns its history inside the alternate screen, so the
 * wheel (or a touch drag) is forwarded to the PTY as mouse reports — the
 * custom wheel handler lifts `disableStdin` for its synchronous dispatch and
 * the onData filter forwards only that wheel-marked input. Keystrokes still
 * require capture; read-only viewers keep xterm's local scroll only.
 */
export function LiveTerminal({
  id,
  agent,
  active = true,
  onEnded,
  privateSessions = [],
  viewerIsAuthor = false,
  viewerCanWrite = false,
  cliAttached = true,
  privateHostId,
}: Props) {
  // The terminal box: clips (windowed) or scrolls (fullscreen) around the grid,
  // and owns the touch-drag scroll gesture.
  const boxRef = React.useRef<HTMLDivElement>(null);
  // Outer box: measured for available width; its height tracks the scaled grid.
  const outerRef = React.useRef<HTMLDivElement>(null);
  // Inner box: xterm mounts here and renders at the source grid; we scale it.
  const screenRef = React.useRef<HTMLDivElement>(null);
  const [conn, setConn] = React.useState<Conn>("connecting");
  // Why a private terminal stopped, for this host and session only. Following
  // it again (after an unlock when the host refused) is always a click.
  const followKey = `${privateHostId}:${id}`;
  const [privateStop, setPrivateStop] = React.useState<{
    key: string;
    message: string;
    unlock: boolean;
  } | null>(null);
  const [privateAttempt, setPrivateAttempt] = React.useState(0);
  const [refollowing, setRefollowing] = React.useState(false);
  const [loading, setLoading] = React.useState(true);
  const [readingHistory, setReadingHistory] = React.useState(false);
  const readerRef = React.useRef<Terminal | null>(null);
  const resumeLiveRef = React.useRef<() => void>(() => {});
  const [capturing, setCapturing] = React.useState(false);
  const isTouch = isTouchDevice();
  const [sessionSearchOpen, setSessionSearchOpen] = React.useState(false);
  const { data: privateGeneration } = useQuery({
    queryKey: privateTerminalGenerationKey(privateHostId, id),
    queryFn: () => 0,
    initialData: 0,
    enabled: false,
  });
  // "Zoom to fit": scale the letters (by font size) so the whole grid fills
  // the available area in both width AND height — growing past the base font
  // when there is room. Viewer-only: the grid itself never changes. Off = fit
  // width only at the source-grid-accurate base size.
  //
  // Default ON for touch devices (phone / tablet): the source grid is typically
  // far taller than a short mobile viewport — especially fullscreen in
  // landscape — so fitting to the screen by default means the whole terminal is
  // visible without the user hunting for the toggle. Desktop keeps actual-size.
  const [fit, setFit] = React.useState(() => isTouch);
  const preferredFontPx = usePreferredFontSize();

  // Fullscreen lives in the URL (`?fullscreen=1`) so it survives a session
  // switch — navigating between live sessions remounts this component, and a
  // local state would reset to windowed. URL-driven, the new session's terminal
  // mounts fullscreen too, which is what the in-overlay switcher needs.
  const [searchParams, setSearchParams] = useSearchParams();
  const location = useLocation();
  const fitNewSession =
    (location.state as { fitNewSession?: string } | null)?.fitNewSession === id;
  const fullscreen = active && searchParams.get("fullscreen") === "1";
  const setFullscreen = React.useCallback(
    (next: boolean) => {
      setSearchParams(
        (prev) => {
          const params = new URLSearchParams(prev);
          if (next) params.set("fullscreen", "1");
          else params.delete("fullscreen");
          return params;
        },
        { replace: true, state: location.state },
      );
    },
    [setSearchParams, location.state],
  );

  const { toast } = useToast();

  // Focus xterm's hidden helper textarea, enabling stdin first so iOS doesn't
  // see a read-only field and refuse the soft keyboard. MUST be called from
  // inside a user gesture (a tap) — that is the whole reason the soft keyboard
  // appears at all; a focus from an effect is ignored by mobile Safari.
  const focusForInput = React.useCallback(() => {
    if (readerRef.current) resumeLiveRef.current();
    const term = termRef.current;
    if (!term) return;
    if (term.options) term.options.disableStdin = false;
    // Keep browser focus from panning the fullscreen container to xterm's
    // hidden input when switching sessions. The public API is a fallback.
    (term.textarea as HTMLTextAreaElement | undefined)?.focus?.({
      preventScroll: true,
    });
    term.focus?.();
  }, []);

  // The private-terminal channel controller for the current (re)connect
  // attempt; held in a ref so the once-per-mount handlers reach the latest
  // channel without re-subscribing.
  const ptyControllerRef = React.useRef<LivePtyController | null>(null);

  // Keystrokes are NOT sent over the terminal socket. They are signed with the
  // browser's temporary key and checked by the host against its local grants
  // (see `createInputChannel`); the socket only carries the screen to us.
  const inputChannelRef = React.useRef<InputChannel | null>(null);
  const toastRef = React.useRef(toast);
  toastRef.current = toast;
  React.useEffect(() => {
    let lastShown = "";
    const channel = createInputChannel(
      id,
      (error) => {
        // One notice per distinct failure, not one per keystroke.
        if (error.message === lastShown) return;
        lastShown = error.message;
        toastRef.current(error.message, "error");
      },
      privateHostId,
    );
    inputChannelRef.current = channel;
    return () => {
      channel.close();
      inputChannelRef.current = null;
    };
  }, [id, privateHostId]);

  const postRawInput = React.useCallback((seq: string) => {
    inputChannelRef.current?.send(seq);
  }, []);

  // Send a raw key sequence straight to the session (used by the touch key
  // bar). Best-effort, mirroring the capture `onData` path.
  const sendKey = React.useCallback(
    (seq: string) => {
      postRawInput(seq);
    },
    [postRawInput],
  );

  // Refs so the stream callbacks don't re-create the terminal or churn state.
  const dimsRef = React.useRef<{ cols: number; rows: number } | null>(null);
  const sawOutputRef = React.useRef(false);
  // Root element, so we can bring the terminal into view once it goes live.
  const rootRef = React.useRef<HTMLDivElement | null>(null);
  const scrolledIntoViewRef = React.useRef(false);
  const onEndedRef = React.useRef(onEnded);
  onEndedRef.current = onEnded;

  // The grid request still waiting for the session to redraw at its size.
  const gridRequestRef = React.useRef<GridRequest | null>(null);
  const settleGridRequest = React.useCallback(() => {
    const pending = gridRequestRef.current;
    if (pending?.timer != null) window.clearTimeout(pending.timer);
    gridRequestRef.current = null;
  }, []);
  React.useEffect(() => settleGridRequest, [id, settleGridRequest]);

  // Called for every frame with the source grid it carries.
  const observeSourceGrid = React.useCallback(
    (cols: number, rows: number) => {
      const pending = gridRequestRef.current;
      if (!pending || pending.cols !== cols || pending.rows !== rows) return;
      if (pending.timer == null) {
        pending.seen = true;
        return;
      }
      settleGridRequest();
      toast(`Terminal is now ${cols}×${rows}.`);
    },
    [settleGridRequest, toast],
  );
  const observeSourceGridRef = React.useRef(observeSourceGrid);
  observeSourceGridRef.current = observeSourceGrid;

  // Measure the terminal box as it stands RIGHT NOW and ask the CLI host for a
  // source grid that fills it at the font it will be read at. Reads
  // `fullscreenRef` rather than the `fullscreen` render value so it can also run
  // a frame after entering fullscreen, when the closure value would still say
  // "windowed". Never touches zoom: the two scaling modes stay independent.
  const postFittedGrid = React.useCallback(
    (quiet = false) => {
      const term = termRef.current;
      const outer = outerRef.current;
      const screen = screenRef.current;
      const viewport = outer?.parentElement as HTMLElement | null;
      const el = term?.element as HTMLElement | undefined;
      const dims =
        dimsRef.current ??
        (term && term.cols > 0 && term.rows > 0
          ? { cols: term.cols, rows: term.rows }
          : null);
      if (!term || !outer || !screen || !viewport || !el || !dims) {
        if (!quiet)
          toast("Waiting for terminal dimensions before resizing.", "error");
        return false;
      }

      const availableWidth = viewport.clientWidth || outer.clientWidth;
      let availableHeight = fullscreenRef.current ? viewport.clientHeight : 0;
      if (availableHeight <= 0) availableHeight = windowedFitHeight(viewport);

      const currentFontPx =
        typeof term.options.fontSize === "number"
          ? term.options.fontSize
          : baseFontRef.current;
      const grid = proposeResizeGrid({
        cols: dims.cols,
        rows: dims.rows,
        renderedWidth: el.offsetWidth,
        renderedHeight: el.offsetHeight,
        currentFontPx,
        availableWidth,
        availableHeight,
        targetFontPx:
          isTouchDevice() && fitRef.current && preferredFontPx === null
            ? RESIZE_TARGET_FONT_PX
            : baseFontRef.current,
      });
      if (!grid) {
        if (!quiet) toast("Could not measure the terminal size yet.", "error");
        return false;
      }
      if (grid.cols === dims.cols && grid.rows === dims.rows) {
        if (!quiet)
          toast(
            `Terminal already fits this screen at ${grid.cols}×${grid.rows}.`,
          );
        return true;
      }

      settleGridRequest();
      const request: GridRequest | null = quiet
        ? null
        : { ...grid, seen: false, timer: null };
      gridRequestRef.current = request;
      // Only the latest request reports; a newer one supersedes this answer.
      const current = () =>
        request !== null && gridRequestRef.current === request;
      void hostControl
        .resizeSession(id, grid.cols, grid.rows, privateHostId)
        .then(() => {
          const pending = gridRequestRef.current;
          if (!current() || !pending) return;
          if (pending.seen) {
            settleGridRequest();
            toast(`Terminal is now ${grid.cols}×${grid.rows}.`);
            return;
          }
          pending.timer = window.setTimeout(() => {
            if (!current()) return;
            gridRequestRef.current = null;
            const now = dimsRef.current ?? dims;
            const unconfirmed = `Resize to ${grid.cols}×${grid.rows} was not confirmed. Current size is ${now.cols}×${now.rows}`;
            const unchanged = now.cols === dims.cols && now.rows === dims.rows;
            toast(
              unchanged
                ? `${unconfirmed}; an attached terminal may control it.`
                : `${unconfirmed}.`,
            );
          }, GRID_CONFIRM_TIMEOUT_MS);
        })
        .catch((err: Error) => {
          if (!current()) return;
          settleGridRequest();
          toast(`Could not resize terminal: ${err.message}`, "error");
        });
      return true;
    },
    [id, preferredFontPx, privateHostId, settleGridRequest, toast],
  );

  // Fit the source grid to the screen the viewer will actually READ it on. On a
  // phone that is always the fullscreen overlay: a tap on the terminal promotes
  // to fullscreen anyway, so fitting the windowed card first produced a grid
  // sized for a fraction of the screen — the session then rendered as a short
  // block of text at the top of the overlay with dead space below it, and no
  // way to grow back. Enter fullscreen first and measure once the browser has
  // committed that layout (two frames: one for React's commit, one for layout).
  // The deferred measurement is two frames out, so it can outlive what it was
  // measuring for: dropped back out of fullscreen, switched session, unmounted.
  // Hold the frame handle so those transitions can cancel it, and re-check
  // fullscreen before posting — a resize aimed at an overlay that is gone would
  // saddle the session with a grid nobody asked for.
  const pendingFitRafRef = React.useRef<number | null>(null);
  const cancelPendingFit = React.useCallback(() => {
    if (pendingFitRafRef.current != null) {
      cancelAnimationFrame(pendingFitRafRef.current);
      pendingFitRafRef.current = null;
    }
  }, []);
  React.useEffect(() => cancelPendingFit, [id, cancelPendingFit]);

  const resizeTerminalToScreen = React.useCallback(() => {
    if (isTouchDevice() && !fullscreenRef.current) {
      setFullscreen(true);
      cancelPendingFit();
      pendingFitRafRef.current = requestAnimationFrame(() => {
        pendingFitRafRef.current = requestAnimationFrame(() => {
          pendingFitRafRef.current = null;
          if (fullscreenRef.current) postFittedGrid();
        });
      });
      return;
    }
    cancelPendingFit();
    postFittedGrid();
  }, [cancelPendingFit, postFittedGrid, setFullscreen]);

  const autoFitIdRef = React.useRef<string | null>(null);
  React.useEffect(() => {
    if (!fitNewSession || !active || loading || autoFitIdRef.current === id)
      return;
    // New sessions opened from the browser should start at the viewer's grid.
    // Wait for replay and layout, and use fullscreen before measuring on touch.
    if (isTouchDevice() && !fullscreenRef.current) setFullscreen(true);
    let frame: number;
    let attempts = 0;
    const fitWhenMeasured = () => {
      if (postFittedGrid(true)) {
        autoFitIdRef.current = id;
      } else if (++attempts < 10) {
        frame = requestAnimationFrame(fitWhenMeasured);
      }
    };
    frame = requestAnimationFrame(() => {
      frame = requestAnimationFrame(fitWhenMeasured);
    });
    return () => {
      cancelAnimationFrame(frame);
    };
  }, [id, fitNewSession, active, loading, postFittedGrid, setFullscreen]);

  // The viewer's saved inline-code colour (per-user setting), used by the
  // Slack-friendly copy handler. Shared cache key with Settings / HostActions.
  // A localStorage override still wins over this (dev escape hatch); both fall
  // back to the baked-in Claude colour. Held in a ref so the copy handler
  // (attached once per mount) always reads the latest without re-subscribing.
  const { data: userSettings } = useQuery({
    queryKey: ["user-settings"],
    queryFn: api.getUserSettings,
    staleTime: 5 * 60_000,
  });
  const codeColorsRef = React.useRef<CodeColorConfig>(DEFAULT_CODE_COLORS);
  codeColorsRef.current = resolveCodeColors(
    undefined,
    codeColorsFromDto(userSettings?.inline_code_colors) ?? DEFAULT_CODE_COLORS,
  );

  // Default GitHub org used by the copy handler to turn a bare `repo#n` PR
  // reference into a link. Inferred (zero-config) from the org of a PR already
  // linked to this session; a `localStorage.arrakisGithubOrg` override wins.
  // Shares the `sessionPrsKey` cache with the PR rail/card. Held in a ref so the
  // once-per-mount copy handler always reads the latest.
  // A privately followed session is not the server's to describe: the org
  // falls back to the `localStorage.arrakisGithubOrg` override.
  const githubOrgRef = React.useRef<string | undefined>(undefined);
  githubOrgRef.current = resolveGithubOrg(undefined, undefined);

  // The live terminal instance + a rescale fn the effects share.
  const termRef = React.useRef<Terminal | null>(null);
  const rescaleRef = React.useRef<() => void>(() => {});
  // The active WebGL renderer addon (windowed only — see `attachWebgl`). Null
  // while the DOM renderer is in use (fullscreen, or before the first attach).
  const webglRef = React.useRef<WebglAddon | null>(null);
  // Latches once WebGL is found unavailable (jsdom test env, GPU-less host) so we
  // stop retrying and stay on the DOM renderer for the rest of the session.
  const webglUnavailableRef = React.useRef(false);
  // Read by the once-per-terminal lifecycle callbacks. Keeping this in a ref
  // lets `active` change without recreating xterm or reconnecting its socket.
  const activeRef = React.useRef(active);
  activeRef.current = active;
  // Font changes resize xterm's canvas. Keep WebGL detached until the scaling
  // loop settles so the addon cannot retain an event-less invalid drawing buffer.
  const fontScaleInProgressRef = React.useRef(false);
  // The active onData subscription while capturing (disposed when off).
  const onDataRef = React.useRef<IDisposable | null>(null);
  // Mirror of `capturing` read by the custom key handler, which is attached
  // once per mount (in the `[id]` effect) and can't close over fresh state.
  const capturingRef = React.useRef(false);
  // The xterm wheel hook and onData callback run synchronously in the same event
  // dispatch. This marks input produced by a wheel gesture without parsing or
  // re-encoding xterm's active mouse protocol (SGR mouse or arrow fallback).
  const forwardingWheelRef = React.useRef(false);
  // Set while a touch drag is being turned into terminal scrolling, so the
  // click the same gesture ends with doesn't arm capture (and raise the soft
  // keyboard) on someone who was only looking back through the session.
  const touchScrolledRef = React.useRef(false);
  const agentRef = React.useRef(agent);
  agentRef.current = agent;
  // Whether this viewer may ask the host to repaint. The server gates /repaint
  // like raw input (author / `can_write`), so a read-only viewer would just
  // 403 — skip the request for them. Read via a ref so the stream effect (keyed
  // on `id`) sees the current value without re-subscribing.
  // Whether this viewer may drive the session: author or a read+write sharee,
  // with a CLI host attached and the stream up. Gates keyboard capture AND
  // wheel-scroll forwarding (the same gate the server enforces on the input
  // channel). Computed here — before the capture/wheel effects that close
  // over it — and mirrored into a ref for the once-per-mount wheel handler,
  // which can't close over fresh state.
  const canCapture =
    active &&
    (viewerIsAuthor || viewerCanWrite) &&
    cliAttached &&
    (conn === "live" || conn === "connecting");
  const canWheelRef = React.useRef(false);
  canWheelRef.current = canCapture;
  // Dropping a file is itself explicit input, so keyboard capture is not needed,
  // but the viewer must be able to write and the session must be live.
  const dropBlockedReason = !active
    ? "Switch to this terminal before dropping files."
    : !(viewerIsAuthor || viewerCanWrite)
      ? "You have read-only access to this session. Ask the session author for write access to attach files."
      : !cliAttached
        ? "No CLI host is attached to this session. Reattach the host, then drop the files again."
        : readingHistory
          ? "Jump to live before dropping files."
          : conn !== "live"
            ? "The terminal isn't live yet. Wait for it to connect, then drop the files again."
            : null;
  const fileDrop = useTerminalFileDrop({
    sessionId: id,
    hostId: privateHostId,
    blockedReason: dropBlockedReason,
    onSent: () => termRef.current?.focus?.(),
  });
  // The rescale closure is created once per mount (in the `[id]` effect) and
  // can't close over fresh `fit`/`fullscreen`; mirror them into refs it reads.
  const fitRef = React.useRef(fit);
  fitRef.current = fit;
  const baseFontRef = React.useRef(preferredFontPx ?? BASE_FONT_PX);
  baseFontRef.current = preferredFontPx ?? BASE_FONT_PX;
  const fullscreenRef = React.useRef(fullscreen);
  fullscreenRef.current = fullscreen;

  // Force a full repaint of whatever renderer is currently up. Neither xterm
  // renderer redraws the existing buffer without new output, so after a renderer
  // swap or a WebGL buffer loss an idle screen stays black until the next byte —
  // this heals it. No-op under the jsdom xterm mock (no `refresh`).
  const repaintNow = React.useCallback(() => {
    const term = termRef.current;
    if (term && typeof term.refresh === "function") {
      term.refresh(0, Math.max(0, term.rows - 1));
    }
  }, []);

  // Attach xterm's WebGL (GPU) renderer for low-latency echo — but ONLY when
  // windowed. Entering the fullscreen `position: fixed` overlay moves the
  // terminal onto a new compositor layer, which silently drops the WebGL drawing
  // buffer with NO `webglcontextlost` event — the canvas goes black and stays
  // black through any refresh / resize / atlas-clear (only recreating the addon
  // in a non-fixed layout recovers it). So fullscreen uses xterm's DOM renderer
  // (immune to this) and we swap back to WebGL on returning to windowed; the
  // renderer effect below drives the swap. Idempotent + guarded for the jsdom
  // test env / GPU-less hosts (a throw latches DOM-only for the session).
  const attachWebgl = React.useCallback(() => {
    const term = termRef.current;
    if (
      !term ||
      !activeRef.current ||
      webglRef.current ||
      webglUnavailableRef.current ||
      fontScaleInProgressRef.current
    ) {
      return;
    }
    try {
      const webgl = new WebglAddon();
      // A real (GPU-reset / browser-forced, e.g. too many live contexts) loss
      // disposes → reverts to DOM; null the ref so a later windowed re-attach can
      // try a fresh context. xterm does NOT repaint the existing buffer on that
      // fallback, so force a repaint on the next frame or the screen stays black
      // until the next byte. (The event-less losses — fullscreen/backgrounded
      // tab — are handled by the renderer + visibility effects below.)
      webgl.onContextLoss(() => {
        webgl.dispose();
        webglRef.current = null;
        requestAnimationFrame(repaintNow);
      });
      term.loadAddon(webgl);
      webglRef.current = webgl;
      // Re-fit against the metrics the GPU renderer actually draws with.
      requestAnimationFrame(() => rescaleRef.current());
    } catch {
      // No GL here — keep xterm's default DOM renderer and never retry.
      webglUnavailableRef.current = true;
    }
  }, [repaintNow]);

  // Drop the WebGL renderer, reverting xterm to its DOM renderer (which CAN
  // present in the fullscreen overlay). Idempotent — a no-op when already on DOM.
  const detachWebgl = React.useCallback(() => {
    const webgl = webglRef.current;
    if (!webgl) return;
    webglRef.current = null;
    try {
      webgl.dispose();
    } catch {
      // already disposed (e.g. via onContextLoss)
    }
  }, []);

  React.useEffect(() => {
    const screen = screenRef.current;
    const outer = outerRef.current;
    if (!screen || !outer) return;

    // Reset per-mount state (id changed → fresh session).
    dimsRef.current = null;
    sawOutputRef.current = false;
    scrolledIntoViewRef.current = false;
    setConn("connecting");
    setLoading(true);
    setReadingHistory(false);
    setCapturing(false);

    const term = createLinkedTerminal({
      convertEol: false,
      disableStdin: true,
      theme: {
        background: "#0d1117",
        foreground: "#c9d1d9",
        cursor: "#58a6ff",
        selectionBackground: "#264f78",
      },
      fontSize: baseFontRef.current,
      fontFamily: '"JetBrains Mono", "Fira Code", "Cascadia Code", monospace',
      scrollback: 5000,
      // The live mirror receives full-screen repaint frames (`CSI 2 J`) from
      // the host to heal drift. Preserve the erased viewport in scrollback so a
      // repaint does not make the browser look like it has no terminal history.
      scrollOnEraseInDisplay: true,
      cols: 80,
      rows: 24,
    });
    const serializeAddon = new SerializeAddon();
    term.loadAddon(serializeAddon);
    term.open(screen);
    termRef.current = term;
    let readerHost: HTMLDivElement | null = null;
    const readingView = createTerminalReadingView(
      term,
      () => serializeAddon.serialize(),
      () => {
        readerHost = document.createElement("div");
        readerHost.style.visibility = "hidden";
        screen.appendChild(readerHost);
        const reader = createLinkedTerminal({
          ...term.options,
          cols: term.cols,
          rows: term.rows,
          disableStdin: true,
        });
        reader.open(readerHost);
        return reader;
      },
      (reader) => {
        readerRef.current = reader;
        setReadingHistory(reader !== null);
        if (reader) setCapturing(false);
        if (term.element) term.element.style.display = reader ? "none" : "";
        if (reader && readerHost) readerHost.style.visibility = "visible";
        if (!reader) {
          readerHost?.remove();
          readerHost = null;
          term.refresh?.(0, term.rows - 1);
        }
        rescaleRef.current();
      },
    );
    resumeLiveRef.current = () => readingView.resume();

    // The renderer (WebGL when windowed for low-latency echo, DOM in the
    // fullscreen overlay where WebGL can't present) is attached by the dedicated
    // renderer effect below — it runs right after this mount and on every
    // fullscreen toggle / session switch, so the terminal opens on the DOM
    // renderer and the effect immediately swaps in WebGL if windowed.

    // Shift+Enter must insert a newline in claude, not submit. xterm emits a
    // bare CR for both Enter and Shift+Enter, so we can't tell them apart on
    // the `onData` byte stream — intercept the key event and forward Meta+Enter
    // (`\x1b\r`), claude's newline chord, instead. Only while capturing; the
    // read-only default path never injects input. `attachCustomKeyEventHandler`
    // is absent under the jsdom xterm mock unless the test provides it — guard.
    if (typeof term.attachCustomKeyEventHandler === "function") {
      term.attachCustomKeyEventHandler((e) => {
        if (isBrowserRefreshKey(e) || isBrowserPasteKey(e)) {
          return false;
        }
        if (
          e.type === "keydown" &&
          e.key === "Enter" &&
          e.shiftKey &&
          capturingRef.current
        ) {
          e.preventDefault();
          inputChannelRef.current?.send("\x1b\r");
          return false; // swallow xterm's default CR
        }
        return true;
      });
    }

    // xterm invokes this immediately before it turns a browser wheel event into
    // terminal input. Keep xterm in charge of protocol/coordinate encoding, but
    // mark the synchronous onData payload so the onData filter below forwards
    // it (and OpenCode can accelerate it). Clear in a microtask because tiny
    // trackpad deltas may not emit input at all.
    //
    // xterm drops ALL onData — mouse reports included — while `disableStdin`
    // is set (`CoreService.triggerDataEvent` returns early). A viewer with
    // write access must be able to scroll WITHOUT arming keyboard capture, so
    // lift `disableStdin` for just this synchronous wheel dispatch; the
    // microtask restore runs before any keystroke could slip through.
    if (typeof term.attachCustomWheelEventHandler === "function") {
      term.attachCustomWheelEventHandler(() => {
        const liftStdin = canWheelRef.current && !capturingRef.current;
        if (liftStdin && term.options) term.options.disableStdin = false;
        forwardingWheelRef.current = true;
        queueMicrotask(() => {
          forwardingWheelRef.current = false;
          if (liftStdin && !capturingRef.current && term.options) {
            term.options.disableStdin = true;
          }
        });
        return true;
      });
    }

    // The font size we last forced a full repaint at. A font change resizes
    // (and thus CLEARS) the WebGL renderer's canvas, but the GPU renderer only
    // paints NEW output — unlike the DOM renderer it does NOT redraw the
    // existing buffer on a resize. So we repaint once the font settles at a new
    // size (see the stable branch below). This covers the WINDOWED WebGL case
    // (e.g. the browser window narrows and the font scales down); fullscreen runs
    // on the DOM renderer, which repaints itself. Per-mount; null until settle.
    let paintedFontSize: number | null = null;
    let measuredTerminal = term;
    let fitBounds = "";
    let firstOverflowFont = Infinity;
    let webglRatio = { w: 1, h: 1 };
    let domMeasure: { key: string; w: number; h: number } | null = null;

    // Fit the rendered terminal to the container WIDTH by adjusting the font
    // size — NOT a CSS transform (see the sizing-strategy note above; a
    // transform breaks text selection). One step per call; if the font changed,
    // the new size renders next frame, so we re-run on rAF to settle and reserve
    // the natural (un-transformed) height.
    const rescale = () => {
      if (!activeRef.current) return;
      const displayed = readerRef.current ?? term;
      const el = displayed.element as HTMLElement | undefined;
      if (!el || !outer) return;
      const natW = el.offsetWidth;
      const natH = el.offsetHeight;
      if (!natW || !natH) return;
      if (!outer.clientWidth) return;

      // The grid is fitted on the DOM renderer (WebGL is detached while the
      // font changes) but the final frame is drawn by WebGL, whose cells have
      // different metrics. Learn the WebGL/DOM size ratio at one font size and
      // apply it to DOM measurements, so the font is chosen for the grid as it
      // will actually render.
      let fitW = natW;
      let fitH = natH;
      if (displayed === term) {
        const measureKey = `${displayed.options.fontSize}:${displayed.cols}:${displayed.rows}`;
        if (webglRef.current) {
          if (domMeasure?.key === measureKey) {
            const ratioW = natW / domMeasure.w;
            const ratioH = natH / domMeasure.h;
            if (ratioW > 0.5 && ratioW < 1.5 && ratioH > 0.5 && ratioH < 1.5) {
              webglRatio = { w: ratioW, h: ratioH };
            }
          }
        } else {
          domMeasure = { key: measureKey, w: natW, h: natH };
          if (!fullscreenRef.current && !webglUnavailableRef.current) {
            fitW = natW * webglRatio.w;
            fitH = natH * webglRatio.h;
          }
        }
      }
      const availW = widthBesideScrollbar(
        outer,
        fullscreenRef.current ? boxRef.current : null,
      );
      if (!availW) return;
      const cur =
        typeof displayed.options.fontSize === "number"
          ? displayed.options.fontSize
          : baseFontRef.current;

      // "Zoom to fit" raises the cap (scale UP to fill the screen) and adds a
      // height constraint. Fullscreen fills its fixed-height box. Windowed, the
      // box height tracks the terminal, so the bound is the window below the
      // terminal's top edge instead — the same area the grid action fits.
      const fitOn = fitRef.current;
      const baseFont = baseFontRef.current;
      const maxFont = fitOn
        ? Math.max(baseFont, isTouch ? MAX_TOUCH_FONT_PX : MAX_FONT_PX)
        : baseFont;
      const constrainH = fitOn;
      const availH = !constrainH
        ? 0
        : fullscreenRef.current
          ? (outer.parentElement?.clientHeight ?? 0)
          : windowedFitHeight(outer);

      const overflowsW = fitW > availW;
      const overflowsH = constrainH && availH > 0 && fitH > availH;

      // Device-pixel cell rounding can make the next font wider than a linear
      // estimate predicts. Do not retry a measured overflow for unchanged bounds.
      const bounds = `${availW}:${availH}:${displayed.cols}:${displayed.rows}:${maxFont}:${fullscreenRef.current}`;
      if (displayed !== measuredTerminal || bounds !== fitBounds) {
        measuredTerminal = displayed;
        fitBounds = bounds;
        firstOverflowFont = Infinity;
      }
      if (overflowsW || overflowsH) {
        firstOverflowFont = Math.min(firstOverflowFont, cur);
      }
      const growthCap = Math.min(maxFont, firstOverflowFont - 1);
      let next = cur;
      if (cur > maxFont) {
        // Switching back to actual size must undo a prior scale-up even when
        // the enlarged grid still fits the available width.
        next = maxFont;
      } else if (overflowsW || overflowsH) {
        // Overflowing an axis: shrink to the tighter fitting size in one jump.
        // Cell geometry scales ~linearly with font size, so estimate from the
        // current render, clamped to the floor.
        const byW = (cur * availW) / fitW;
        const byH = overflowsH ? (cur * availH) / fitH : Infinity;
        next = Math.max(MIN_FONT_PX, Math.floor(Math.min(byW, byH)));
      } else if (cur < growthCap) {
        // Try one predicted fitting step; a measured overflow bounds later tries.
        const predictedW = (fitW * (cur + 1)) / cur;
        const predictedH = (fitH * (cur + 1)) / cur;
        const fitsW = predictedW <= availW;
        const fitsH = !constrainH || availH <= 0 || predictedH <= availH;
        if (fitsW && fitsH) next = Math.min(growthCap, cur + 1);
      }

      if (next !== cur) {
        // Changing xterm's font resizes its canvas. A plain refresh usually
        // repaints WebGL afterwards, but Chromium can instead leave the addon
        // holding an invalid drawing buffer without firing `webglcontextlost`;
        // that is the scaled-down black-terminal regression. Use the DOM
        // renderer throughout the geometry transition, then attach a fresh
        // WebGL addon once the size has settled below.
        if (!fontScaleInProgressRef.current) {
          fontScaleInProgressRef.current = true;
          detachWebgl();
        }
        displayed.options.fontSize = next;
        requestAnimationFrame(() => rescaleRef.current());
        return;
      }
      // Stable: reserve the terminal's natural height (no transform in play).
      outer.style.height = `${natH}px`;

      // A windowed resize that shrinks the font (e.g. the browser window
      // narrows) CLEARS the WebGL canvas, but the GPU renderer won't redraw the
      // existing buffer without new output, so an idle session's screen goes
      // blank. Force a full repaint once — and only when the font actually
      // changed since the last paint, so steady-state frame writes (which
      // repaint themselves) don't trigger redundant refreshes. Harmless on the
      // DOM renderer (fullscreen). Guarded for the jsdom xterm mock.
      if (cur !== paintedFontSize) {
        paintedFontSize = cur;
        if (typeof term.refresh === "function") {
          term.refresh(0, Math.max(0, term.rows - 1));
        }
      }

      if (fontScaleInProgressRef.current) {
        fontScaleInProgressRef.current = false;
        // Paint the existing buffer on the DOM renderer first. Re-create WebGL
        // on the following frame, after xterm and layout have committed the final
        // cell geometry, then paint once more into the new drawing buffer.
        repaintNow();
        requestAnimationFrame(() => {
          if (
            termRef.current !== term ||
            fullscreenRef.current ||
            fontScaleInProgressRef.current
          ) {
            return;
          }
          attachWebgl();
          repaintNow();
        });
      }
    };
    rescaleRef.current = rescale;

    // jsdom has no ResizeObserver; guard so tests don't blow up.
    let ro: ResizeObserver | undefined;
    if (typeof ResizeObserver !== "undefined") {
      ro = new ResizeObserver(() => rescale());
      ro.observe(outer);
    }
    // A taller or shorter window changes what a zoomed windowed terminal can
    // fill without changing the terminal's own width.
    const onWindowResize = () => rescale();
    window.addEventListener("resize", onWindowResize);

    const controller = new AbortController();
    let connectAttempt = 0;
    let ended = false;
    let socketAuthorized = false;
    let initializationStarted = false;
    let initialized = false;
    let terminalDisposed = false;
    const terminalOperations = createTerminalFrameQueue();
    let authoritativeReplay: Array<{
      cols: number;
      rows: number;
      bytes: Uint8Array;
    }> = [];
    type PendingEvent =
      | {
          type: "frame";
          cols: number;
          rows: number;
          bytes: Uint8Array;
        }
      | { type: "replay_complete" };
    const pendingEvents: PendingEvent[] = [];

    // xterm parses write data asynchronously. A resize or reset performed while
    // an earlier write is still being parsed changes the geometry/state under
    // that write, which can wrap bytes at the wrong width or apply them twice
    // around a replay boundary. Keep every buffer mutation in one callback-led
    // queue; the next operation starts only after xterm confirms the prior write.
    const enqueueTerminalOperation = (
      operation: (done: () => void) => void,
    ) => {
      terminalOperations.enqueue(operation);
    };

    const enqueueReset = () => {
      enqueueTerminalOperation((done) => {
        readingView.beforeWrite(() => {
          term.reset();
          dimsRef.current = null;
          done();
        });
      });
    };

    const applyFrame = (cols: number, rows: number, bytes: Uint8Array) => {
      terminalOperations.frame(
        { cols, rows, bytes },
        ({ cols, rows, bytes }, _count, done) => {
          readingView.beforeWrite(() => {
            const anchor = captureTerminalViewport(term);
            const cur = dimsRef.current;
            // Compute dimensions when this frame reaches the head of the queue. A
            // prior reset or write may still be pending when the frame is received.
            const dimsChanged = !cur || cur.cols !== cols || cur.rows !== rows;
            if (dimsChanged) {
              term.resize(cols, rows);
              dimsRef.current = { cols, rows };
            }
            observeSourceGridRef.current(cols, rows);
            const pinned = isPinnedToBottom(term);
            term.write(bytes, () => {
              if (!terminalDisposed) {
                if (dimsChanged) rescale();
                if (pinned) term.scrollToBottom?.();
                else restoreTerminalViewport(term, anchor);
              }
              done();
            });
          });
        },
      );
      if (!sawOutputRef.current) {
        sawOutputRef.current = true;
        setConn("live");
      }
    };

    //
    // The server's explicit replay-complete marker—not the first PTY frame—is
    // the boundary. Raw history spans many frames; treating frame one as
    // complete made a switch/drop halfway through replay permanently truncate
    // the beginning on the next connection.
    let replayComplete = false;
    const processEvent = (event: PendingEvent) => {
      if (event.type === "frame") {
        if (!replayComplete) {
          authoritativeReplay.push(event);
          if (!sawOutputRef.current) {
            sawOutputRef.current = true;
            setConn("live");
          }
          return;
        }
        applyFrame(event.cols, event.rows, event.bytes);
        return;
      }

      replayComplete = true;
      const replay = authoritativeReplay;
      authoritativeReplay = [];
      replay.forEach((frame) =>
        applyFrame(frame.cols, frame.rows, frame.bytes),
      );
      const completingAttempt = connectAttempt;
      enqueueTerminalOperation((done) => {
        if (
          !terminalDisposed &&
          !ended &&
          completingAttempt === connectAttempt
        ) {
          setLoading(false);
          setConn("live");
        }
        done();
      });
    };

    const flushPendingEvents = () => {
      if (!initialized) return;
      pendingEvents.splice(0).forEach(processEvent);
    };

    const acceptEvent = (event: PendingEvent) => {
      if (!initialized) pendingEvents.push(event);
      else processEvent(event);
    };

    // A private screen is never written to browser storage, so there is no
    // cache to hydrate: initialize as soon as the socket authorized.
    const initializeFromCache = () => {
      if (initialized || initializationStarted || ended || !socketAuthorized) {
        return;
      }
      initializationStarted = true;
      initialized = true;
      flushPendingEvents();
    };

    const handlers = {
      // A fresh xterm mount should trust the server's initial replay first. An
      // eager host repaint here can be harmful when the local CLI is older than
      // the server: its visible-screen-only keyframe may arrive immediately
      // after the replay and clear the scrollback the mount was supposed to
      // restore. Only ask the host for a keyframe after an in-page stream drop,
      // where the existing xterm buffer may have diverged.
      onOpen() {
        socketAuthorized = true;
        initializeFromCache();
      },
      onReplayComplete() {
        acceptEvent({ type: "replay_complete" });
      },
      // The server saw an input frame reach zero CLI subscribers (no host
      // attached), so stop keyboard capture — the old `{delivered:false}`
      // behavior, now pushed rather than returned per-keystroke.
      onInputDropped() {
        setCapturing(false);
      },
      onFrame(cols: number, rows: number, bytes: Uint8Array) {
        acceptEvent({ type: "frame", cols, rows, bytes });
      },
    };

    // Self-reconnecting PTY stream. A clean EOF means the source stopped (claude
    // exited / hub closed on session-complete) and a 404 means the session is no
    // longer live — both terminal. A thrown transport error (server redeploy
    // SIGTERMs the pod → the SSE connection RSTs; or a network blip) is NOT
    // terminal: the host stays up across deploys and re-pushes the current screen
    // when its control stream re-attaches, so we just reconnect.
    // On the first connection after each xterm mount, the server replays raw PTY
    // history into the blank buffer so scrollback survives reloads and tab
    // switches. On reconnects of that same mounted xterm it replays the
    // SELF-CLEARING snapshot
    // (`ESC[H ESC[2J ESC[3J` + history/current screen), so we must NOT
    // `term.reset()` here — doing so would only discard the scrollback the
    // replay is about to restore, and the replay overwrites any stale grid.
    const markEnded = () => {
      ended = true;
      setConn("ended");
      setLoading(false);
      setCapturing(false);
      onEndedRef.current?.();
    };
    void (async () => {
      for (;;) {
        // The per-(re)connect repaint request now fires from `handlers.onOpen`
        // — once the socket is subscribed — so the healing keyframe reaches this
        // viewer live instead of only the server snapshot (see the note there).
        const pty = openPrivatePty(
          privateHostId,
          id,
          handlers,
          controller.signal,
        );
        ptyControllerRef.current = pty;
        try {
          const end = await pty.closed;
          if (end.reason === "aborted") return;
          markEnded(); // clean socket close — session done
          return;
        } catch (error) {
          if (controller.signal.aborted) return;
          // Privately, only a dropped connection is worth retrying: a refusal
          // or a missing capability would be refused again.
          if (!(
            error instanceof HostControlError && error.code === "transport"
          )) {
            ended = true;
            const message = (error as Error)?.message ?? String(error);
            setConn("error");
            setLoading(false);
            setCapturing(false);
            setPrivateStop({
              key: `${privateHostId}:${id}`,
              message,
              unlock:
                error instanceof PrivateTerminalEnded
                  ? error.reason === "denied"
                  : error instanceof HostControlError &&
                    error.code === "refused",
            });
            return;
          }
          connectAttempt += 1;
          // If the initial history replay dropped before its explicit completion
          // marker, discard the partial xterm buffer before retrying the raw
          // history. Raw replay is intentionally not self-clearing; keeping the
          // partial buffer would duplicate its prefix on retry.
          if (!replayComplete) {
            enqueueReset();
          }
          authoritativeReplay = [];
          // Transient socket drop (server redeploy SIGTERMs the pod → the WS
          // RSTs; or a network blip). The host stays up across deploys and
          // re-pushes the current screen when its control stream re-attaches, so
          // we reconnect. Drop back to "connecting" and retry after a backoff.
          // The replayed snapshot is self-clearing and repaints history + screen,
          // so we keep the existing buffer (no reset) and let it overwrite.
          sawOutputRef.current = false;
          dimsRef.current = null;
          setConn("connecting");
          setLoading(true);
          await sleep(PTY_RECONNECT_BACKOFF_MS);
        }
      }
    })();

    return () => {
      controller.abort();
      terminalDisposed = true;
      terminalOperations.dispose();
      readingView.dispose();
      readerHost?.remove();
      readerRef.current = null;
      resumeLiveRef.current = () => {};
      ptyControllerRef.current = null;
      ro?.disconnect();
      window.removeEventListener("resize", onWindowResize);
      onDataRef.current?.dispose();
      onDataRef.current = null;
      // `term.dispose()` disposes loaded addons (incl. WebGL); just drop our ref
      // so the renderer effect re-attaches cleanly to the next session's term.
      webglRef.current = null;
      fontScaleInProgressRef.current = false;
      term.dispose();
      termRef.current = null;
      rescaleRef.current = () => {};
      dimsRef.current = null;
    };
  }, [
    attachWebgl,
    detachWebgl,
    id,
    isTouch,
    privateHostId,
    privateAttempt,
    privateGeneration,
    repaintNow,
  ]);

  // Landing on a session while already fullscreen means the author is here to
  // drive it — so auto-arm keyboard capture. Include author + CLI presence
  // because a just-started session can mount before the live-session poll has
  // delivered that metadata; when it arrives, capture must still arm and focus.
  React.useEffect(() => {
    if (active && fullscreen && viewerIsAuthor && cliAttached) {
      setCapturing(true);
    }
  }, [id, active, fullscreen, viewerIsAuthor, cliAttached]);

  // On a phone or tablet, keyboard capture is the primary interaction mode and
  // the soft keyboard consumes much of the viewport. Promote every capture
  // path (terminal tap or menu action, for authors and write sharees alike) to
  // fullscreen so the remaining space belongs to the live session.
  React.useEffect(() => {
    if (active && isTouch && capturing && !fullscreen) setFullscreen(true);
  }, [active, isTouch, capturing, fullscreen, setFullscreen]);

  // Keep the renderer in sync with fullscreen: WebGL when windowed (low-latency
  // echo) and xterm's DOM renderer in the fullscreen overlay, where WebGL can't
  // present in the `position: fixed` compositor layer (see `attachWebgl`). Runs
  // on mount (so the terminal picks up WebGL once windowed) and on every
  // fullscreen toggle / session switch (`id`). Declared before the rescale effect
  // so the renderer is already correct when the fit measures the element.
  React.useEffect(() => {
    const term = termRef.current;
    if (!term) return;

    if (!active) {
      // Hidden pooled terminals stay on the DOM renderer. Keeping one WebGL
      // context per visited session quickly hits browser GPU-context limits.
      detachWebgl();
      return;
    }

    if (fullscreen) {
      // Fullscreen overlay → DOM renderer. Drop WebGL first so the immediate
      // repaint lands on a renderer that can actually present in the fixed layer.
      detachWebgl();
      repaintNow();
      const raf = requestAnimationFrame(() => {
        repaintNow();
        rescaleRef.current();
      });
      return () => cancelAnimationFrame(raf);
    }

    // Windowed → WebGL, but attach only AFTER the layout has settled back into
    // the static (non-fixed) layer. Attaching during a fullscreen→windowed
    // transition creates the GL context against a compositor layer that is torn
    // down as the overlay unwinds, silently dropping the fresh drawing buffer
    // with NO webglcontextlost event → a black windowed terminal that no repaint
    // can revive. So drop any stale addon, repaint on the DOM renderer meanwhile,
    // and attach a fresh WebGL context on the next frame, then repaint + re-fit.
    detachWebgl();
    repaintNow();
    const raf = requestAnimationFrame(() => {
      attachWebgl();
      repaintNow();
      rescaleRef.current();
    });
    return () => cancelAnimationFrame(raf);
  }, [id, active, fullscreen, attachWebgl, detachWebgl, repaintNow]);

  // Heal the WebGL buffer losses that fire NO webglcontextlost event and so slip
  // past `attachWebgl`'s loss handler: a backgrounded tab whose GPU context the
  // browser discards comes back black on refocus. On becoming visible again,
  // repaint immediately, then re-establish a fresh renderer for the current mode
  // on the next frame (windowed re-attaches WebGL; fullscreen stays on DOM).
  React.useEffect(() => {
    const onVisibility = () => {
      if (!activeRef.current) return;
      if (document.visibilityState !== "visible") return;
      if (!termRef.current) return;
      repaintNow();
      requestAnimationFrame(() => {
        if (!termRef.current) return;
        detachWebgl();
        if (!fullscreenRef.current) attachWebgl();
        repaintNow();
      });
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => document.removeEventListener("visibilitychange", onVisibility);
  }, [attachWebgl, detachWebgl, repaintNow]);

  // Recompute the scale-to-fit whenever we enter/leave fullscreen (the outer
  // container's available size changes) or toggle "Zoom to fit" (the cap and
  // height constraint change); rAF lets layout settle first.
  React.useEffect(() => {
    if (!active) return;
    const raf = requestAnimationFrame(() => rescaleRef.current());
    return () => cancelAnimationFrame(raf);
  }, [active, fullscreen, fit, preferredFontPx]);

  // Touch drag → terminal scrolling, in the fullscreen overlay.
  //
  // xterm 6 bundles a touch-gesture helper but never binds it to a target, and
  // its scrollable `.xterm-viewport` sits UNDERNEATH the `.xterm-screen` layer a
  // finger actually lands on — so a phone drag never reaches xterm's scrollback.
  // What it reaches instead is the nearest scrollable ancestor: the PAGE behind
  // this fixed overlay. That is what a phone user sees — the window scrolls, the
  // session doesn't.
  //
  // The gesture has to be claimed BEFORE the browser starts that scroll:
  //   - `touch-action: pan-x` on the box (set in the render below) tells the
  //     browser not to handle vertical panning for touches starting here at all.
  //     Without it the first uncancelled `touchmove` commits the gesture to the
  //     page and every later `preventDefault` is ignored (the event even stops
  //     being `cancelable`), which is exactly how the earlier attempt failed.
  //   - `preventDefault` on EVERY move we own, not just the ones that cross the
  //     notch threshold, for the same reason.
  // Horizontal panning is deliberately left to the browser, so a grid wider than
  // the screen can still be panned sideways.
  //
  // Then move the terminal, not the box:
  //   - alternate screen (OpenCode's TUI owns its own history): dispatch the
  //     wheel event xterm turns into a mouse report for the session, the same
  //     path a trackpad takes.
  //   - normal screen: `scrollLines` xterm's own viewport directly — no
  //     dependence on xterm's wheel plumbing or scroll sensitivity.
  // A box that overflows vertically (an unfitted grid taller than the screen)
  // pans itself first, since the rows below the fold are unreachable otherwise.
  // On release, recent finger velocity decays through requestAnimationFrame so a
  // flick coasts instead of stopping at the last discrete touchmove.
  React.useEffect(() => {
    const box = boxRef.current;
    if (!box) return;

    let lastY: number | null = null;
    let startX = 0;
    let startY = 0;
    let carry = 0;
    let velocity = 0;
    let lastMoveAt = 0;
    let lastFrameAt = 0;
    let momentumFrame: number | null = null;
    // Which way this gesture went, decided once (see below). "x" is the
    // browser's to pan; "y" is ours.
    let axis: "x" | "y" | null = null;

    const stopMomentum = (clearCarry = true) => {
      if (momentumFrame !== null) cancelAnimationFrame(momentumFrame);
      momentumFrame = null;
      velocity = 0;
      lastFrameAt = 0;
      if (clearCarry) carry = 0;
    };

    const applyScrollDelta = (dy: number) => {
      const term = readerRef.current ?? termRef.current;
      if (!term || dy === 0) return;

      // An oversized grid: pan the box to the rows below the fold first, and
      // hand on whatever the pan couldn't consume — at either edge the box
      // clamps, and the rest of the drag belongs to the terminal.
      let remaining = dy;
      if (box.scrollHeight > box.clientHeight + 1) {
        const before = box.scrollTop;
        box.scrollTop = before - remaining;
        remaining -= before - box.scrollTop;
        if (remaining === 0) return;
      }

      carry += remaining;
      const alternate = term.buffer?.active?.type === "alternate";
      const el = term.element as HTMLElement | undefined;
      let notches = 0;
      while (
        Math.abs(carry) >= TOUCH_SCROLL_STEP_PX &&
        notches < TOUCH_SCROLL_MAX_NOTCHES_PER_MOVE
      ) {
        const sign = Math.sign(carry);
        carry -= sign * TOUCH_SCROLL_STEP_PX;
        // Dragging DOWN pulls earlier output into view: a wheel-UP (negative
        // deltaY) / a scroll back by one row.
        if (alternate) {
          el?.dispatchEvent(
            new WheelEvent("wheel", {
              deltaY: -sign * TOUCH_SCROLL_STEP_PX,
              deltaMode: 0, // DOM_DELTA_PIXEL — xterm converts px to rows
              bubbles: true,
              cancelable: true,
            }),
          );
        } else {
          term.scrollLines?.(-sign);
        }
        notches++;
      }
    };

    const glide = (frameAt: number) => {
      momentumFrame = null;
      if (!fullscreenRef.current) {
        stopMomentum();
        return;
      }

      const elapsed = frameAt - lastFrameAt;
      if (elapsed <= 0) {
        momentumFrame = requestAnimationFrame(glide);
        return;
      }
      if (elapsed > TOUCH_SCROLL_MOMENTUM_MAX_FRAME_GAP_MS) {
        stopMomentum();
        return;
      }
      lastFrameAt = frameAt;

      const nextVelocity =
        velocity * Math.exp(-elapsed / TOUCH_SCROLL_MOMENTUM_DECAY_MS);
      // Integrate the decay across this frame, avoiding an initial speed-up or
      // a lurch near the tail.
      applyScrollDelta(((velocity + nextVelocity) / 2) * elapsed);
      velocity = nextVelocity;

      if (Math.abs(velocity) < TOUCH_SCROLL_MOMENTUM_STOP_SPEED_PX_MS) {
        stopMomentum();
        return;
      }
      momentumFrame = requestAnimationFrame(glide);
    };

    const onTouchStart = (e: TouchEvent) => {
      stopMomentum();
      touchScrolledRef.current = false;
      axis = null;
      const single = e.touches.length === 1;
      lastY = single ? e.touches[0].clientY : null;
      startX = single ? e.touches[0].clientX : 0;
      startY = lastY ?? 0;
      lastMoveAt = single ? performance.now() : 0;
    };

    const onTouchMove = (e: TouchEvent) => {
      const term = readerRef.current ?? termRef.current;
      if (lastY == null || e.touches.length !== 1 || !term) return;
      // Windowed, a drag over the terminal belongs to the page.
      if (!fullscreenRef.current) return;

      const y = e.touches[0].clientY;
      const dy = y - lastY;
      lastY = y;

      // Pick the axis on the FIRST move that has any delta, and keep it for the
      // rest of the gesture. There is no room for an "undecided" window:
      // cancelling a move stops the browser from ever scrolling this gesture,
      // and NOT cancelling lets it start one we can no longer take back. Ties
      // (and a purely vertical drag, which reports no horizontal delta) go to
      // us — vertical is the gesture this exists for.
      if (axis === null) {
        const totalX = Math.abs(e.touches[0].clientX - startX);
        const totalY = Math.abs(y - startY);
        if (totalX === 0 && totalY === 0) return;
        axis = totalX > totalY ? "x" : "y";
      }
      // Sideways: leave it to `touch-action: pan-x` so a grid wider than the
      // screen can still be panned.
      if (axis === "x") return;

      // Ours from here: claim it even before the drag adds up to a notch.
      if (e.cancelable) e.preventDefault();
      if (dy === 0) return;
      touchScrolledRef.current = true;

      const now = performance.now();
      const elapsed = now - lastMoveAt;
      if (elapsed > 0 && elapsed <= TOUCH_SCROLL_MOMENTUM_STALE_MS) {
        const sampled = Math.max(
          -TOUCH_SCROLL_MAX_SPEED_PX_MS,
          Math.min(TOUCH_SCROLL_MAX_SPEED_PX_MS, dy / elapsed),
        );
        // Favor the fresh movement but smooth timing noise between touch events.
        velocity = velocity * 0.35 + sampled * 0.65;
      } else {
        velocity = 0;
      }
      lastMoveAt = now;

      applyScrollDelta(dy);
    };

    const onTouchEnd = () => {
      lastY = null;
      const fresh =
        performance.now() - lastMoveAt <= TOUCH_SCROLL_MOMENTUM_STALE_MS;
      if (
        axis === "y" &&
        fresh &&
        Math.abs(velocity) >= TOUCH_SCROLL_MOMENTUM_STOP_SPEED_PX_MS
      ) {
        lastFrameAt = performance.now();
        momentumFrame = requestAnimationFrame(glide);
      } else {
        stopMomentum();
      }
    };

    const onTouchCancel = () => {
      lastY = null;
      stopMomentum();
    };

    box.addEventListener("touchstart", onTouchStart, { passive: true });
    box.addEventListener("touchmove", onTouchMove, { passive: false });
    box.addEventListener("touchend", onTouchEnd, { passive: true });
    box.addEventListener("touchcancel", onTouchCancel, { passive: true });
    return () => {
      stopMomentum();
      box.removeEventListener("touchstart", onTouchStart);
      box.removeEventListener("touchmove", onTouchMove);
      box.removeEventListener("touchend", onTouchEnd);
      box.removeEventListener("touchcancel", onTouchCancel);
    };
  }, [fullscreen]);

  // Keep the page still while the overlay is up. The overlay is `position:
  // fixed`, so on a phone the document behind it stays scrollable and a drag
  // anywhere outside the terminal box (header, key bar) scrolls the page under
  // the session instead of doing nothing.
  React.useEffect(() => {
    if (!fullscreen) return;
    const root = document.documentElement;
    const body = document.body;
    const prev = {
      rootOverflow: root.style.overflow,
      bodyOverflow: body.style.overflow,
      overscroll: body.style.overscrollBehavior,
    };
    root.style.overflow = "hidden";
    body.style.overflow = "hidden";
    body.style.overscrollBehavior = "none";
    return () => {
      root.style.overflow = prev.rootOverflow;
      body.style.overflow = prev.bodyOverflow;
      body.style.overscrollBehavior = prev.overscroll;
    };
  }, [fullscreen]);

  // Wire input forwarding. A viewer with write access (`canCapture`: author or
  // a read+write sharee, CLI attached, stream up) gets an always-on onData
  // subscription so WHEEL gestures reach the session even with keyboard
  // capture off — stdin stays disabled, and the custom wheel handler lifts it
  // for just its synchronous wheel dispatch. Keystrokes still require capture,
  // which the toolbar toggle / terminal click gates on the same `canCapture`.
  React.useEffect(() => {
    capturingRef.current = capturing;
    const term = termRef.current;
    // `options`/`onData`/`focus` are absent under the jsdom xterm mock; guard
    // so the read-only default path (capturing=false) never throws in tests.
    if (!term || typeof term.onData !== "function") return;

    if (!capturing && !canCapture) {
      onDataRef.current?.dispose();
      onDataRef.current = null;
      term.options.disableStdin = true;
      // Drop focus so the mobile soft keyboard dismisses when capture stops.
      (term.textarea as HTMLTextAreaElement | undefined)?.blur?.();
      return;
    }

    // Stdin follows CAPTURE only — a write viewer's keyboard must not reach
    // the session while capture is off (the wheel path re-enables stdin
    // transiently, one dispatch at a time).
    term.options.disableStdin = !capturing;
    if (capturing) {
      focusForInput();
    } else {
      // Capture just stopped: drop focus so the mobile soft keyboard dismisses.
      (term.textarea as HTMLTextAreaElement | undefined)?.blur?.();
    }
    const forwardMouse = createLinkClickFilter(term);
    const sub = term.onData((data) => {
      // Capture off: forward only wheel-generated input (marked synchronously
      // by the custom wheel handler) — never keystrokes. Capture on: every
      // keystroke is forwarded, including a bare ESC (claude uses it to
      // interrupt / clear the prompt) and multi-char escape sequences (arrow
      // keys `\x1b[A`, etc.). Capture is stopped via the toolbar toggle, not
      // the keyboard.
      if (!capturingRef.current && !forwardingWheelRef.current) return;
      if (!forwardMouse(data)) return;
      const forwarded =
        agentRef.current === "opencode" && forwardingWheelRef.current
          ? data.repeat(OPENCODE_WHEEL_INPUT_MULTIPLIER)
          : data;
      postRawInput(forwarded);
    });
    onDataRef.current = sub;

    return () => {
      sub.dispose();
      if (onDataRef.current === sub) onDataRef.current = null;
    };
  }, [capturing, canCapture, focusForInput, postRawInput]);

  // Capture-mode focus fallback. xterm normally receives keyboard input through
  // its hidden textarea, but a session switch/new-session navigation can leave
  // focus on the toolbar button or document body while `capturing` is already
  // true. In that state the UI says "Capturing keyboard" but xterm emits no
  // onData frames. While capture is armed, translate document keydowns only when
  // the active element is NOT xterm's textarea or another editable field.
  React.useEffect(() => {
    if (!capturing) return;

    const onKeyDown = (e: KeyboardEvent) => {
      if (
        document.querySelector(
          '[role="dialog"], [role="tooltip"], [role="menu"]',
        )
      )
        return;
      const termTextarea = termRef.current?.textarea as
        HTMLTextAreaElement | undefined;
      if (activeElementConsumesText(termTextarea)) return;

      const seq = keyboardEventToTerminalSequence(e);
      if (seq == null) return;

      e.preventDefault();
      e.stopPropagation();
      postRawInput(seq);
    };

    document.addEventListener("keydown", onKeyDown, true);
    return () => document.removeEventListener("keydown", onKeyDown, true);
  }, [capturing, postRawInput]);

  const pasteImage = React.useCallback(
    async (image: Blob, mime: string): Promise<boolean> => {
      if (image.size > LIVE_CLIPBOARD_MAX_BYTES) {
        toastRef.current(
          `This image is larger than ${LIVE_CLIPBOARD_MAX_BYTES / 1024 / 1024} MiB. Nothing was sent.`,
          "error",
        );
        return false;
      }
      try {
        const bytes = new Uint8Array(await image.arrayBuffer());
        await hostControl.pasteImage(id, mime, bytes, {}, privateHostId);
        return true;
      } catch (error) {
        toastRef.current(pasteFailureMessage(error), "error");
        return false;
      }
    },
    [id, privateHostId],
  );

  // While capturing, a pasted image is shipped to the host instead of going
  // through xterm (which only handles text): the host stages the image in this
  // session's private directory and pastes its path (never Enter) so the agent
  // attaches the file. The host's machine clipboard is not used. Listen on the document
  // (the paste lands on xterm's hidden textarea and bubbles) and intercept the
  // first image item. Non-image pastes fall through to xterm's normal text path.
  React.useEffect(() => {
    if (!capturing) return;
    const onPaste = (e: ClipboardEvent) => {
      if (
        e.target instanceof Element &&
        e.target.closest("[data-terminal-paste]")
      )
        return;
      const items = e.clipboardData?.items;
      if (!items) return;
      for (let i = 0; i < items.length; i++) {
        const item = items[i];
        if (!item.type.startsWith("image/")) continue;
        const file = item.getAsFile();
        if (!file) continue;
        e.preventDefault();
        e.stopPropagation();
        void pasteImage(file, item.type);
        return;
      }
    };
    document.addEventListener("paste", onPaste, true);
    return () => document.removeEventListener("paste", onPaste, true);
  }, [capturing, pasteImage]);

  // Copy: enrich xterm's plain-text copy with Slack-friendly formatting. xterm's
  // own copy handler (bubble phase, on `term.element`) writes only `text/plain`,
  // dropping the per-cell colour that is the live terminal's ONLY "this is code"
  // signal. This capture-phase listener on the mount container (an ancestor of
  // `term.element`) runs first: it writes both a backtick `text/plain` flavour
  // and a `<code>` `text/html` flavour that Slack's composer reads, then stops
  // the event so xterm's handler can't overwrite the plain flavour. Any failure
  // bows out without `preventDefault`, leaving xterm's default copy intact — and
  // until the inline-code colour is pinned (see `termClipboard.ts`) the payload
  // is plain text identical to today, so this never regresses copy. Works for
  // read-only viewers too (copy needs no capture/write access).
  React.useEffect(() => {
    const screen = screenRef.current;
    if (!screen) return;
    const onCopy = (e: ClipboardEvent) => {
      const term = (readerRef.current ??
        termRef.current) as unknown as TermLike | null;
      if (!term || !e.clipboardData) return;
      if (typeof term.getSelectionPosition !== "function") return;
      try {
        if (isClipboardDebugEnabled()) {
          const sigs = selectionColorSignatures(term);
          // Discovery aid: read this table, pin the dominant code colour into
          // `localStorage.arrakisTermCodeColors`. Gated behind a debug flag.
          if (sigs.length) console.table(sigs);
        }
        const payload = serializeTermSelection(term, codeColorsRef.current, {
          org: githubOrgRef.current,
        });
        if (!payload || payload.text.length === 0) return;
        e.clipboardData.setData("text/plain", payload.text);
        e.clipboardData.setData("text/html", payload.html);
        e.preventDefault();
        e.stopPropagation();
      } catch {
        // Leave xterm's default plain-text copy in place.
      }
    };
    screen.addEventListener("copy", onCopy, true);
    return () => screen.removeEventListener("copy", onCopy, true);
  }, [id]);

  // In fullscreen, ESC leaves fullscreen — but ONLY when keyboard capture is
  // off. While capturing, ESC belongs to the session: it's forwarded by the
  // xterm `onData` handler (claude interrupt / clear), so we must not intercept
  // it here — stop capture from the toolbar toggle first. Once capture is off, a
  // lone ESC exits fullscreen. Handled on the document in the capture phase so
  // it pre-empts any residual xterm handling.
  React.useEffect(() => {
    if (!fullscreen || capturing) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (
        document.querySelector(
          '[role="dialog"], [role="tooltip"], [role="menu"]',
        )
      )
        return;
      e.preventDefault();
      e.stopPropagation();
      setFullscreen(false);
    };
    document.addEventListener("keydown", onKey, true);
    return () => document.removeEventListener("keydown", onKey, true);
  }, [fullscreen, capturing, setFullscreen]);

  // Windowed counterpart to click-to-capture: a click OUTSIDE the terminal
  // releases keyboard capture, so the viewer's next keystrokes go to the rest
  // of the page (the chat box, a link, another field) instead of being
  // swallowed by the terminal. "Outside" is measured against the whole widget
  // (`rootRef`), NOT just the screen — so the terminal's own toolbar (Scale,
  // resize, fullscreen, End session…) and the touch on-screen key bar don't
  // count as outside and drop capture mid-use. Disabled in FULLSCREEN: the
  // overlay fills the viewport (there's nothing meaningful to click away to) and
  // capture is released there via the toolbar toggle / ESC instead. Listen on
  // pointerdown (covers mouse, touch, and pen, and fires before focus moves) in
  // the capture phase so it pre-empts any same-gesture handling.
  React.useEffect(() => {
    if (!capturing || fullscreen) return;
    const onPointerDown = (e: PointerEvent) => {
      const root = rootRef.current;
      // Session menus are portalled outside the terminal's DOM subtree.
      if (e.target instanceof Element && e.target.closest('[role="menu"]'))
        return;
      if (root && !root.contains(e.target as Node)) setCapturing(false);
    };
    document.addEventListener("pointerdown", onPointerDown, true);
    return () =>
      document.removeEventListener("pointerdown", onPointerDown, true);
  }, [capturing, fullscreen]);

  // When the stream first goes live, bring the terminal into view — on the
  // session/live page it's often below the fold, so the response would
  // otherwise appear off-screen. Once per session (reset on id change).
  React.useEffect(() => {
    if (!active || conn !== "live" || scrolledIntoViewRef.current) return;
    scrolledIntoViewRef.current = true;
    rootRef.current?.scrollIntoView?.({ behavior: "smooth", block: "end" });
  }, [active, conn]);

  // If write access is lost while already capturing (e.g. a share is downgraded
  // to read-only mid-session), stop capturing — otherwise the onData/paste
  // handlers keep intercepting input after the toggle has disappeared.
  React.useEffect(() => {
    if (!canCapture && capturing) setCapturing(false);
  }, [canCapture, capturing]);

  // Touch device → enable the tap-to-type affordance + on-screen key bar.
  // Read at render so tests can drive it via `navigator.maxTouchPoints`.
  const [narrowViewport, setNarrowViewport] = React.useState(() =>
    typeof window.matchMedia === "function"
      ? window.matchMedia("(max-width: 767px)").matches
      : false,
  );
  React.useEffect(() => {
    if (typeof window.matchMedia !== "function") return;
    const query = window.matchMedia("(max-width: 767px)");
    const update = (event: MediaQueryListEvent) =>
      setNarrowViewport(event.matches);
    setNarrowViewport(query.matches);
    query.addEventListener("change", update);
    return () => query.removeEventListener("change", update);
  }, []);
  const compactToolbar = isTouch || narrowViewport;

  const hint =
    conn === "connecting"
      ? "Waiting for terminal output… (is `conversations live` running with an up-to-date CLI?)"
      : conn === "error"
        ? "Live terminal unavailable for this session."
        : null;

  const stopped = privateStop?.key === followKey ? privateStop : null;
  const followAgain = async (unlock: boolean) => {
    if (!privateHostId) return;
    setRefollowing(true);
    try {
      if (unlock) await unlockAgain(privateHostId);
      setPrivateStop(null);
      setConn("connecting");
      setLoading(true);
      setPrivateAttempt((n) => n + 1);
    } catch (error) {
      toastRef.current((error as Error)?.message ?? String(error), "error");
    } finally {
      setRefollowing(false);
    }
  };

  const [diagramsOpen, setDiagramsOpen] = React.useState(false);
  const diagramMarkers = useTerminalDiagramMarkers(
    () => readerRef.current ?? termRef.current,
    outerRef,
    active,
    `${id}:${privateHostId}:${privateAttempt}:${privateGeneration}:${isTouch}:${readingHistory}`,
  );

  const display: DisplayControls = {
    canResize: viewerIsAuthor || viewerCanWrite,
    fit,
    onToggleFit: () => setFit((value) => !value),
    fullscreen,
    // Entering fullscreen as the host (author) auto-arms keyboard
    // capture: a host going fullscreen is there to drive the session.
    // Leaving it on touch drops capture, as there is no key to hold.
    onToggleFullscreen: () => {
      const next = !fullscreen;
      setFullscreen(next);
      if (next && viewerIsAuthor && cliAttached) setCapturing(true);
      if (!next && isTouch) setCapturing(false);
    },
    onResize: resizeTerminalToScreen,
    // Only an active (live) session has diagrams to show. Gating by `active`
    // also keeps the menu from offering an item that opens an empty dialog on
    // an ended session.
    onOpenDiagrams: active ? () => setDiagramsOpen(true) : undefined,
  };

  return (
    <div
      ref={rootRef}
      className={
        fullscreen
          ? "fixed inset-0 z-60 flex flex-col bg-[#0d1117] py-1 pl-1 pr-0 sm:p-4"
          : "rounded-xl border border-border bg-card py-1 pl-1 pr-0 sm:p-4"
      }
    >
      <div
        // Keep both groups together until they no longer fit; wrap controls above tabs.
        className="mb-3 flex flex-wrap-reverse items-end justify-between gap-2"
        data-testid="session-compact-row"
      >
        <div className="flex min-w-0 max-w-full grow basis-max items-center gap-2">
          {/* Tabs identify the active session (name + live-activity dot +
              private marker for private tabs), carry the working-directory
              tooltip, and are the primary switcher. They replace the big
              session header on every surface — windowed and fullscreen alike. */}
          <AllHostSessionTabs
            key={privateHostId}
            sessions={privateSessions}
            hostId={privateHostId}
            currentId={id}
          />
          {readingHistory && (
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => resumeLiveRef.current()}
            >
              Jump to live
            </Button>
          )}
          {loading && (
            <span
              role="status"
              className="flex items-center gap-2 text-xs text-muted-foreground"
            >
              <span
                aria-hidden="true"
                className="size-3 animate-spin rounded-full border-2 border-current border-t-transparent"
              />
              Loading terminal…
            </span>
          )}
        </div>
        <div className="ml-auto flex max-w-full flex-wrap items-center justify-end gap-2">
          {/* The AppShell header (and its bell) sits under the fullscreen
              overlay, so fullscreen re-surfaces notifications here. The
              Settings gear lives at the very end of this toolbar — see the
              closing block below. */}
          {fullscreen && !compactToolbar && <Notifications />}
          {/* Clicking the terminal arms capture, so only the way out needs a
              control: ESC belongs to the session, so the keyboard cannot release. */}
          {isTouch && canCapture && (
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => {
                setCapturing(true);
                focusForInput();
              }}
            >
              Type
            </Button>
          )}
          {!compactToolbar && canCapture && capturing && (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              aria-label="Keyboard captured. Release keyboard"
              title="Keyboard captured. Click to release it."
              onClick={() => setCapturing(false)}
              className="gap-1.5 text-muted-foreground hover:text-foreground"
            >
              <span
                aria-hidden
                className="size-1.5 rounded-full bg-emerald-400"
              />
              Keyboard captured
            </Button>
          )}
          {!compactToolbar && (
            <Button
              type="button"
              variant="outline"
              size="sm"
              aria-label="Search sessions"
              title="Search sessions"
              onClick={() => setSessionSearchOpen(true)}
            >
              <svg
                aria-hidden
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                className="h-4 w-4"
              >
                <circle cx="11" cy="11" r="8" />
                <path d="m21 21-4.3-4.3" />
              </svg>
            </Button>
          )}
          {!compactToolbar &&
            privateSessions.some((entry) => entry.id === id) && (
              <NewSessionButton />
            )}
          {privateSessions.find((entry) => entry.id === id) ? (
            <TerminalMenus
              privateSession={{
                hostId: privateHostId,
                entry: privateSessions.find((entry) => entry.id === id)!,
              }}
              sessions={[]}
              currentId={id}
              compact={compactToolbar}
              canCapture={canCapture}
              capturing={capturing}
              focusForInput={focusForInput}
              onToggleCapture={() => setCapturing((value) => !value)}
              display={display}
              onSearchSessions={() => setSessionSearchOpen(true)}
            />
          ) : (
            <DisplayMenu
              capturing={capturing}
              focusForInput={focusForInput}
              display={display}
            />
          )}
          {/* Settings is the rightmost control on every surface. In fullscreen
              the AppShell gear sits under this overlay, so re-surface it here
              as the last item — after Display/Actions — on every width. */}
          {fullscreen && (
            <>
              <HomeLink />
              <SettingsGearLink />
            </>
          )}
        </div>
      </div>

      {/* Diagrams opens from the Display menu. The dialog is controlled by
          state lifted to LiveTerminal so the menu item can close itself and
          hand off to the dialog; keyed on `id` so switching sessions resets
          internal state (which diagram is selected, etc.). Content inside the
          dialog is guarded on `open` in TerminalDiagrams, so live-history is
          only read while the viewer is open. */}
      {active && (
        <TerminalDiagrams
          key={id}
          sessionId={id}
          hostId={privateHostId}
          open={diagramsOpen}
          onOpenChange={setDiagramsOpen}
        />
      )}

      {/* Sit narrow rails to the RIGHT of the terminal — in fullscreen always
          (the page-level PullRequestsCard + LinearDocsCard are hidden under
          the overlay), and in windowed mode too on viewports wide enough to
          seat a 240px rail beside the live terminal (xl: ≥ 1280px); narrow
          viewports keep rendering the page-level cards below instead. Each
          rail renders nothing when nothing is linked, so the column only
          claims width when there's something to show. */}
      <div
        className={
          fullscreen
            ? "flex min-h-0 flex-1 gap-1 sm:gap-3"
            : "flex gap-1 sm:gap-3"
        }
      >
        <div
          ref={boxRef}
          data-testid="live-terminal-screen"
          // Fullscreen: a fixed overlay whose height is the (short, in landscape)
          // viewport. Scroll vertically so the bottom rows stay reachable even
          // when the grid is taller than the viewport — `min-h-0` lets this flex
          // child shrink below its content so the overflow actually engages.
          // Windowed: clip to the rounded card; the page itself scrolls.
          {...fileDrop.handlers}
          className={`rounded-lg min-w-0 flex-1 ${
            fullscreen ? "min-h-0 overflow-auto" : "overflow-hidden"
          }${fileDrop.dragging ? " ring-2 ring-inset ring-emerald-400" : ""}`}
          style={{
            background: "#0d1117",
            // Claim vertical drags in fullscreen BEFORE the browser can start
            // its own scroll. Without this the first `touchmove` commits the
            // gesture to the nearest scrollable ancestor — the page behind the
            // overlay — and no later `preventDefault` can take it back, so the
            // window scrolls and the session sits still. `pan-x` keeps sideways
            // panning of a too-wide grid native. `overscrollBehavior: contain`
            // stops a scroll that reaches the end of the box from chaining out
            // to the page (and kills pull-to-refresh over the terminal).
            ...(fullscreen
              ? { touchAction: "pan-x", overscrollBehavior: "contain" as const }
              : null),
          }}
          // A click on the terminal arms keyboard capture for a viewer who can
          // control it — a host just clicks the screen and starts typing, no
          // toolbar trip. Capture is released from the toolbar status. We also
          // focus xterm's textarea INSIDE the click gesture: on touch that's the
          // only thing that raises the iOS soft keyboard; on desktop it's
          // harmless and lands the first keystroke without a second click.
          // Read-only viewers (no `canCapture`) keep xterm's native
          // click-to-select / scroll — we don't hijack their clicks.
          onClick={
            canCapture
              ? () => {
                  // A drag that scrolled the terminal ends in a click too; that
                  // one is not a request to type.
                  if (touchScrolledRef.current) {
                    touchScrolledRef.current = false;
                    return;
                  }
                  if (readerRef.current) return;
                  if (!capturing) setCapturing(true);
                  focusForInput();
                }
              : undefined
          }
        >
          {(fileDrop.dragging || fileDrop.status) && (
            <div className="pointer-events-none sticky top-0 z-10 h-0">
              <div
                role="status"
                data-testid="terminal-drop-status"
                className="absolute left-1/2 top-3 -translate-x-1/2 rounded-md border border-border bg-card px-3 py-1.5 text-xs font-medium text-foreground shadow-lg"
              >
                {fileDrop.status === "reading"
                  ? "Reading files…"
                  : fileDrop.status === "sending"
                    ? "Sending files to the terminal host…"
                    : (dropBlockedReason ?? "Drop files to attach them")}
              </div>
            </div>
          )}
          {/* Outer box reserves the scaled height; inner box is scaled. */}
          <div ref={outerRef} className="relative w-full">
            <div
              ref={screenRef}
              data-testid="live-terminal-grid"
              className="absolute left-0 top-0"
            />
            {diagramMarkers.map((marker) => (
              <button
                key={marker.key}
                type="button"
                aria-label="View diagram"
                title="View this diagram"
                onClick={(event) => {
                  event.stopPropagation();
                  setDiagramsOpen(true);
                }}
                style={{ left: marker.left, top: marker.top }}
                className="absolute z-10 flex h-6 w-6 items-center justify-center rounded border border-border bg-card/90 text-foreground shadow hover:bg-accent"
              >
                <svg
                  aria-hidden
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="2"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  className="h-4 w-4"
                >
                  <rect x="3" y="3" width="7" height="5" rx="1" />
                  <rect x="14" y="16" width="7" height="5" rx="1" />
                  <path d="M6.5 8v5.5h11V16" />
                </svg>
              </button>
            ))}
          </div>
        </div>
        <div
          data-testid="live-terminal-rail"
          className={`min-h-0 shrink-0 flex-col gap-3 ${
            fullscreen ? "flex" : "hidden xl:flex"
          }`}
        >
          <SessionPrProvider hostId={privateHostId}>
            <LivePrRail sessionId={id} />
          </SessionPrProvider>
          <SessionLinearProvider hostId={privateHostId}>
            <LiveLinearRail sessionId={id} />
          </SessionLinearProvider>
          <CommunicationRail sessionId={id} hostId={privateHostId} />
        </div>
      </div>

      {/* Touch-only on-screen control keys (ESC / Tab / arrows / Enter / ^C) a
          soft keyboard can't send. Shown only while capturing. `preventDefault`
          on press keeps focus on the textarea so the keyboard stays up. */}
      {isTouch && capturing && canCapture && (
        <div className="mt-2 flex flex-wrap gap-1.5">
          {MOBILE_KEYS.map((k) => (
            <button
              key={k.label}
              type="button"
              title={k.title}
              aria-label={k.title}
              onPointerDown={(e) => {
                e.preventDefault();
                sendKey(k.seq);
              }}
              className="min-w-10 rounded-md border border-border bg-card px-2.5 py-2 text-sm font-medium text-foreground transition-colors active:bg-emerald-500/15"
            >
              {k.label}
            </button>
          ))}
          {conn === "live" && (
            <MobileTerminalPaste
              key={id}
              onImage={pasteImage}
              onPaste={(text) => {
                // Use xterm's native paste path so bracketed paste and newline
                // normalization match desktop paste. Do not submit the prompt.
                termRef.current?.paste(text);
                focusForInput();
              }}
            />
          )}
        </div>
      )}
      {hint && <p className="mt-2 text-xs text-muted-foreground">{hint}</p>}
      {conn === "error" && stopped && (
        <div className="mt-2 flex flex-wrap items-center gap-2 text-xs">
          <span className="text-muted-foreground">{stopped.message}</span>
          <Button
            size="sm"
            variant="outline"
            disabled={refollowing}
            onClick={() => void followAgain(stopped.unlock)}
          >
            {stopped.unlock ? "Unlock and follow again" : "Follow again"}
          </Button>
        </div>
      )}
      {canCapture && !capturing && (
        <p className="mt-2 text-xs text-muted-foreground">
          {isTouch ? "Tap" : "Click"} the terminal to type.
        </p>
      )}
      <SessionSearchDialog
        open={sessionSearchOpen}
        onOpenChange={setSessionSearchOpen}
        hostId={privateHostId}
      />
    </div>
  );
}

/**
 * Toolbar "New session" button — asks the viewer's CLI host to spawn a fresh
 * `claude` session (`POST /api/live/start-new`) and, once it registers, opens
 * it (preserving `?fullscreen=1`, so creating from the fullscreen terminal
 * lands you in the new session still fullscreen). Lives in the terminal toolbar
 * so it stays reachable in fullscreen, where the page-level header (and its
 * `StartNewSessionButton`) is hidden under the overlay — the same reason the
 * `LiveTabs` switcher is mirrored here. Mirrors the `Ctrl-A c` hotkey. Disabled
 * with a tooltip when no CLI host is attached.
 */
function NewSessionButton() {
  return (
    <StartNewSessionButton label="New session" size="sm" variant="outline" />
  );
}

interface DisplayControls {
  canResize: boolean;
  fit: boolean;
  onToggleFit: () => void;
  fullscreen: boolean;
  onToggleFullscreen: () => void;
  onResize: () => void;
  /**
   * Open the terminal diagrams viewer. Omitted when the session is not active
   * (the viewer has nothing to render then), so the menu stays honest.
   */
  onOpenDiagrams?: () => void;
}

/** How the terminal is drawn: grid size, zoom, fullscreen, diagrams. */
function DisplayMenuItems({
  canResize,
  fit,
  onToggleFit,
  fullscreen,
  onToggleFullscreen,
  onResize,
  onOpenDiagrams,
}: DisplayControls) {
  return (
    <>
      {canResize && (
        <DropdownMenu.Item
          onSelect={onResize}
          title={FIT_GRID_DESCRIPTION}
          className={sessionMenuItemClass}
        >
          Fit columns and rows to screen
        </DropdownMenu.Item>
      )}
      <DropdownMenu.Item
        onSelect={onToggleFit}
        title={fit ? RESET_ZOOM_DESCRIPTION : ZOOM_TO_FIT_DESCRIPTION}
        className={sessionMenuItemClass}
      >
        {fit ? "Reset zoom" : "Zoom to fit"}
      </DropdownMenu.Item>
      <DropdownMenu.Item
        onSelect={onToggleFullscreen}
        className={sessionMenuItemClass}
      >
        {fullscreen ? "Exit fullscreen" : "Fullscreen"}
      </DropdownMenu.Item>
      {onOpenDiagrams && (
        <DropdownMenu.Item
          onSelect={onOpenDiagrams}
          title="View Mermaid diagrams streamed by the agent"
          className={sessionMenuItemClass}
        >
          Diagrams
        </DropdownMenu.Item>
      )}
    </>
  );
}

/** Opening a menu steals focus from a capturing terminal; hand it back on close. */
function useRefocusAfterMenu(capturing: boolean, focusForInput: () => void) {
  const focusAfterClose = React.useRef(false);
  React.useEffect(() => {
    if (!capturing) focusAfterClose.current = false;
  }, [capturing]);
  return {
    focusAfterClose,
    onOpenChange: (open: boolean) => {
      if (open && capturing) focusAfterClose.current = true;
    },
    onCloseAutoFocus: (event: Event) => {
      if (!focusAfterClose.current) return;
      event.preventDefault();
      focusAfterClose.current = false;
      focusForInput();
    },
  };
}

function DisplayMenu({
  capturing,
  focusForInput,
  display,
}: {
  capturing: boolean;
  focusForInput: () => void;
  display: DisplayControls;
}) {
  const refocus = useRefocusAfterMenu(capturing, focusForInput);
  return (
    <DropdownMenu.Root onOpenChange={refocus.onOpenChange}>
      <DropdownMenu.Trigger asChild>
        <Button
          type="button"
          variant="outline"
          size="sm"
          aria-label="Display"
          title="Display"
          className="shrink-0 px-2"
        >
          <svg
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.8"
            strokeLinecap="round"
            strokeLinejoin="round"
            className="h-4 w-4"
            aria-hidden
          >
            <rect x="3" y="3" width="18" height="13" rx="2" />
            <path d="M12 16v5m-4 0h8" />
          </svg>
        </Button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align="end"
          sideOffset={6}
          onCloseAutoFocus={refocus.onCloseAutoFocus}
          className="z-70 flex min-w-56 flex-col rounded-md border border-border bg-popover p-1 text-sm text-popover-foreground shadow-md"
        >
          <DisplayMenuItems {...display} />
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

interface TerminalMenusProps {
  privateSession?: { hostId: string; entry: SessionEntry };
  session?: Pick<
    LiveSession,
    "id" | "title" | "viewer_is_author" | "viewer_can_write"
  >;
  sessions: LiveSession[];
  currentId: string;
  compact: boolean;
  canCapture: boolean;
  capturing: boolean;
  focusForInput: () => void;
  onToggleCapture: () => void;
  display: DisplayControls;
  onSearchSessions: () => void;
}

/**
 * Desktop: separate Display and Actions menus. Compact screens: one Actions
 * menu that also carries a Display section, capture, new-session targets and
 * navigation.
 */
function TerminalMenus({
  session,
  privateSession,
  sessions,
  currentId,
  compact,
  canCapture,
  capturing,
  focusForInput,
  onToggleCapture,
  display,
  onSearchSessions,
}: TerminalMenusProps) {
  const { available } = useControlAvailable();
  const { start, busy: starting } = useStartNewSession();
  const refocus = useRefocusAfterMenu(capturing, focusForInput);

  const itemClass = sessionMenuItemClass;

  const renderActions = (children: React.ReactNode) =>
    privateSession ? (
      <PrivateSessionActionsMenu
        session={privateSession.entry}
        hostId={privateSession.hostId}
        onOpenChange={refocus.onOpenChange}
        onCloseAutoFocus={refocus.onCloseAutoFocus}
      >
        {compact ? children : undefined}
      </PrivateSessionActionsMenu>
    ) : session ? (
      <SessionActionsMenu
        session={session}
        sessions={sessions}
        currentId={currentId}
        onOpenChange={refocus.onOpenChange}
        onCloseAutoFocus={refocus.onCloseAutoFocus}
      >
        {compact ? children : undefined}
      </SessionActionsMenu>
    ) : null;

  return (
    <>
      {renderActions(
        <>
          {compact ? (
            <>
              <DropdownMenu.Label className="px-2 pb-1 pt-2 text-xs text-muted-foreground">
                Display
              </DropdownMenu.Label>
              <DisplayMenuItems {...display} />
              <DropdownMenu.Separator className="my-1 h-px bg-border" />
              {canCapture && (
                <DropdownMenu.Item
                  onSelect={() => {
                    refocus.focusAfterClose.current = !capturing;
                    onToggleCapture();
                  }}
                  className={itemClass}
                >
                  {capturing ? "Stop capturing keyboard" : "Capture keyboard"}
                </DropdownMenu.Item>
              )}
              <DropdownMenu.Label className="px-2 pb-1 pt-2 text-xs text-muted-foreground">
                New session
              </DropdownMenu.Label>
              {ALL_LAUNCH_TARGETS.map((target: LaunchTarget) => (
                <DropdownMenu.Item
                  key={launchTargetKey(target)}
                  disabled={!available || starting}
                  title={available ? undefined : "No CLI host attached"}
                  onSelect={() => void start(target)}
                  className={itemClass}
                >
                  {starting
                    ? "Starting…"
                    : `New ${launchTargetLabel(target)} session`}
                </DropdownMenu.Item>
              ))}
              <DropdownMenu.Separator className="my-1 h-px bg-border" />
              <DropdownMenu.Item asChild>
                <Link to="/" className={itemClass}>
                  Sessions overview
                </Link>
              </DropdownMenu.Item>
              {
                <DropdownMenu.Item
                  onSelect={onSearchSessions}
                  className={itemClass}
                >
                  Search sessions…
                </DropdownMenu.Item>
              }
            </>
          ) : undefined}
        </>,
      )}
      {!compact && (
        <DisplayMenu
          capturing={capturing}
          focusForInput={focusForInput}
          display={display}
        />
      )}
    </>
  );
}

/** localStorage key for the fullscreen PR rail's collapsed state (a UI
 *  preference, so it carries across sessions and reloads). */
const PR_RAIL_COLLAPSED_KEY = "conversations.prRailCollapsed";

/** localStorage key for the fullscreen Linear rail's collapsed state (its own
 *  preference, independent of the PR rail). */
const LINEAR_RAIL_COLLAPSED_KEY = "conversations.linearRailCollapsed";

/** How many comments to preview per PR in the (narrow) rail before the rest
 *  collapse to a link to the PR. */
const RAIL_PREVIEW_COMMENTS = 2;

/**
 * Compact, fullscreen-only PR rail pinned to the right of the live terminal.
 * Mirrors the page-level {@link PullRequestsCard} (hidden under the fullscreen
 * overlay) but trimmed to a narrow list — a state dot, repo#number (the org is
 * omitted and the repo truncates so it never steals width from the terminal),
 * the title, and a short preview of each PR's latest comments. A header ↻ button
 * relays a sync to the attached CLI, and the whole list collapses behind its
 * header (persisted). Renders nothing while loading or when no PRs are linked,
 * so it only claims space when there's something to show. Shares the
 * `sessionPrsKey` cache with the card, so it stays in sync with the page.
 */
function LivePrRail({ sessionId }: { sessionId: string }) {
  const source = useSessionPrSource();
  const queryClient = useQueryClient();
  const { toast } = useToast();

  const { data: prs } = useQuery({
    queryKey: source.key(sessionPrsKey(sessionId)),
    queryFn: () => source.listSessionPrs(sessionId),
    // A live session may briefly 404 before its first message; don't spam.
    retry: false,
  });

  // Bounded PR comment previews, partitioned per-PR below. Best
  // effort: a failure just hides the previews, it never blocks the rail.
  const { data: comments } = useQuery({
    queryKey: source.key(prCommentPreviewsKey(sessionId)),
    queryFn: () => source.listPrCommentPreviews(sessionId),
    retry: false,
  });

  // Collapsed state persists so it survives reloads / re-entering fullscreen.
  const [collapsed, setCollapsed] = React.useState<boolean>(() => {
    try {
      const stored = localStorage.getItem(PR_RAIL_COLLAPSED_KEY);
      if (stored !== null) return stored === "true";
    } catch {
      // Use the viewport default when storage is unavailable.
    }
    return (
      typeof window.matchMedia === "function" &&
      window.matchMedia("(max-width: 767px)").matches
    );
  });
  const toggleCollapsed = React.useCallback(() => {
    setCollapsed((c) => {
      const next = !c;
      try {
        localStorage.setItem(PR_RAIL_COLLAPSED_KEY, String(next));
      } catch {
        // ignore — quota or private-browsing restriction
      }
      return next;
    });
  }, []);

  // "Sync": relay a sync request to the attached CLI (gh stays on the CLI — the
  // server holds no GitHub creds). On delivery the CLI pushes fresh status +
  // comments; refresh the cached lists shortly after.
  const sync = useMutation({
    mutationFn: () => source.syncPrs(sessionId),
    onSuccess: ({ delivered }) => {
      if (delivered) {
        toast("Syncing PRs…");
        window.setTimeout(() => {
          queryClient.invalidateQueries({
            queryKey: source.key(sessionPrsKey(sessionId)),
          });
          queryClient.invalidateQueries({
            queryKey: source.key(prCommentPreviewsKey(sessionId)),
          });
        }, 2500);
      } else {
        toast(
          "No sync agent connected — run `arrakis-conversations host`",
          "error",
        );
      }
    },
    onError: (err: Error) => {
      // A capability-narrowed delegation is a well-defined, user-fixable
      // state: the owner credential on the host does not hold
      // `session.write` for this session. PR refresh now follows the
      // terminal's write boundary — anyone allowed to control the running
      // terminal may also refresh its pull requests — so the one next step
      // is to add "Control running terminals" to the owner key. Adding it
      // does not re-enroll the passkey. Keep the host's wording so it
      // stays searchable.
      const message = /outside the delegation'?s scope/i.test(err.message)
        ? `${err.message} Open Settings → Access to this host on your laptop (localhost) and add "Control running terminals" to your owner key, then try again.`
        : err.message;
      toast(`Could not sync PRs: ${message}`, "error");
    },
  });

  if (!prs || prs.length === 0) return null;

  return (
    // Collapsed shrinks the rail to a thin strip pinned to the right (handing the
    // width back to the terminal) — not just an empty w-60 box. Expanding restores
    // the full-width list.
    <aside
      className={`flex min-h-0 flex-col rounded-lg border border-border bg-card/60 p-3 ${
        collapsed ? "w-9 items-center" : "w-60"
      }`}
    >
      <div
        className={`flex items-center gap-2 ${collapsed ? "" : "mb-2 justify-between"}`}
      >
        <button
          type="button"
          aria-expanded={!collapsed}
          onClick={toggleCollapsed}
          title={collapsed ? "Expand pull requests" : "Collapse pull requests"}
          className="flex min-w-0 items-center gap-1 text-left"
        >
          <span
            aria-hidden
            className={`text-[0.65rem] text-muted-foreground transition-transform duration-150 ${
              collapsed ? "" : "rotate-90"
            }`}
          >
            ▶
          </span>
          {!collapsed && (
            <span className="truncate text-xs font-semibold text-muted-foreground">
              Pull requests
            </span>
          )}
        </button>
        {!collapsed && (
          <button
            type="button"
            onClick={() => sync.mutate()}
            disabled={sync.isPending}
            aria-label="Sync pull requests"
            title="Sync pull requests"
            className="shrink-0 rounded-md border border-border p-1 text-muted-foreground transition-colors hover:text-foreground disabled:cursor-not-allowed disabled:opacity-60"
          >
            <span
              aria-hidden
              className={`block text-xs leading-none ${sync.isPending ? "animate-spin" : ""}`}
            >
              ↻
            </span>
          </button>
        )}
      </div>

      {collapsed ? (
        // A count badge is the only thing the thin rail shows, so it's clear there
        // are hidden PRs to expand back to.
        <button
          type="button"
          aria-label={`Expand pull requests (${prs.length})`}
          onClick={toggleCollapsed}
          className="mt-1 text-xs font-semibold text-muted-foreground transition-colors hover:text-foreground"
        >
          {prs.length}
        </button>
      ) : (
        <ul className="min-h-0 space-y-2 overflow-y-auto">
          {/* The API returns oldest-first; render a copy as a newest-first stack. */}
          {prs
            .slice()
            .reverse()
            .map((pr) => (
              <RailPrRow
                key={pr.url}
                pr={pr}
                comments={commentsForPr(comments, pr)}
              />
            ))}
        </ul>
      )}
    </aside>
  );
}

/** One PR in the fullscreen rail: repo#number (org omitted, repo truncates) +
 *  title + a short, truncated preview of its latest comments. */
function RailPrRow({ pr, comments }: { pr: SessionPr; comments: PrComment[] }) {
  const shown = comments.slice(0, RAIL_PREVIEW_COMMENTS);
  const extra = comments.length - shown.length;
  return (
    <li className="text-xs">
      <a
        href={safeHref(pr.url)}
        target="_blank"
        rel="noopener noreferrer"
        title={`${pr.owner}/${pr.repo}#${pr.number}`}
        className="flex min-w-0 items-center gap-1.5 font-medium text-foreground hover:text-primary hover:underline"
      >
        <PrStateDot status={pr.status} />
        <span className="flex min-w-0 items-baseline">
          <span className="truncate">{pr.repo}</span>
          <span className="shrink-0 text-muted-foreground">#{pr.number}</span>
        </span>
        <PrReviewIcon status={pr.status} />
      </a>
      {pr.status?.title && (
        <p
          className="mt-0.5 truncate text-[0.7rem] text-muted-foreground"
          title={pr.status.title}
        >
          {pr.status.title}
        </p>
      )}
      {shown.length > 0 && (
        <ul
          className="mt-1 space-y-0.5 border-l border-border/60 pl-2"
          data-testid="rail-pr-comments"
        >
          {shown.map((c) => (
            <RailCommentLine key={c.comment_id} comment={c} />
          ))}
          {extra > 0 && (
            <li className="text-[0.7rem]">
              <a
                href={safeHref(pr.url)}
                target="_blank"
                rel="noopener noreferrer"
                className="text-muted-foreground hover:text-foreground hover:underline"
              >
                More on GitHub
              </a>
            </li>
          )}
        </ul>
      )}
    </li>
  );
}

/** One comment preview in a rail PR row: author + a one-line body. A CodeRabbit
 *  inline finding is prefixed with a {@link CodeRabbitSeverityDot} and previewed
 *  by its bold summary (falling back to the raw first line). */
function RailCommentLine({ comment }: { comment: PrComment }) {
  const isCodeRabbit = isCodeRabbitAuthor(comment.author);
  const severity = isCodeRabbit ? codeRabbitSeverity(comment.body) : null;
  const preview =
    (isCodeRabbit ? codeRabbitPreview(comment.body) : null) ??
    firstLine(comment.body);
  return (
    <li className="min-w-0">
      <a
        href={safeHref(comment.html_url)}
        target="_blank"
        rel="noopener noreferrer"
        title={comment.body}
        className="flex min-w-0 items-center gap-1 text-[0.7rem] text-muted-foreground hover:text-foreground"
      >
        {severity && <CodeRabbitSeverityDot severity={severity} />}
        <span className="min-w-0 truncate">
          <span className="font-medium text-foreground">{comment.author}</span>{" "}
          {preview}
        </span>
      </a>
    </li>
  );
}

/** Tiny colored dot encoding a CodeRabbit finding's severity, keyed off
 *  CodeRabbit's own colored-circle badge (🔴 critical / 🟠 major / 🟡 minor /
 *  🔵 trivial). Mirrors {@link PrStateDot} / {@link LinearKindDot}. */
function CodeRabbitSeverityDot({ severity }: { severity: CodeRabbitSeverity }) {
  const { dotClass, label } = CODE_RABBIT_SEVERITY_META[severity];
  return (
    <span
      aria-label={`severity: ${label}`}
      title={`CodeRabbit severity: ${label}`}
      data-testid="rail-coderabbit-severity"
      className={`inline-block h-1.5 w-1.5 shrink-0 rounded-full ${dotClass}`}
    />
  );
}

/**
 * Compact, fullscreen-only Linear rail pinned to the right of the live terminal,
 * directly below {@link LivePrRail}. The Linear analogue of that rail and the
 * page-level {@link LinearDocsCard} (both hidden under the fullscreen overlay) —
 * a kind dot + identifier per row, no status and no sync (the server holds no
 * Linear creds). Renders nothing when no Linear refs are linked, so it only
 * claims width when there's something to show. Shares the `sessionLinearDocsKey`
 * cache with the card, so the `linear_docs_linked` live nudge refreshes it too.
 */
function LiveLinearRail({ sessionId }: { sessionId: string }) {
  const source = useSessionLinearSource();
  const { data: docs } = useQuery({
    queryKey: source.key(sessionLinearDocsKey(sessionId)),
    queryFn: () => source.list(sessionId),
    // A live session may briefly 404 before its first message; don't spam.
    retry: false,
  });

  const [collapsed, setCollapsed] = React.useState<boolean>(() => {
    try {
      const stored = localStorage.getItem(LINEAR_RAIL_COLLAPSED_KEY);
      if (stored !== null) return stored === "true";
    } catch {
      // Use the viewport default when storage is unavailable.
    }
    return (
      typeof window.matchMedia === "function" &&
      window.matchMedia("(max-width: 767px)").matches
    );
  });
  const toggleCollapsed = React.useCallback(() => {
    setCollapsed((c) => {
      const next = !c;
      try {
        localStorage.setItem(LINEAR_RAIL_COLLAPSED_KEY, String(next));
      } catch {
        // ignore — quota or private-browsing restriction
      }
      return next;
    });
  }, []);

  if (!docs || docs.length === 0) return null;

  return (
    <aside
      className={`flex min-h-0 flex-col rounded-lg border border-border bg-card/60 p-3 ${
        collapsed ? "w-9 items-center" : "w-60"
      }`}
    >
      <button
        type="button"
        aria-expanded={!collapsed}
        onClick={toggleCollapsed}
        title={collapsed ? "Expand Linear" : "Collapse Linear"}
        className={`flex min-w-0 items-center gap-1 text-left ${collapsed ? "" : "mb-2"}`}
      >
        <span
          aria-hidden
          className={`text-[0.65rem] text-muted-foreground transition-transform duration-150 ${
            collapsed ? "" : "rotate-90"
          }`}
        >
          ▶
        </span>
        {!collapsed && (
          <span className="truncate text-xs font-semibold text-muted-foreground">
            Linear
          </span>
        )}
      </button>

      {collapsed ? (
        <button
          type="button"
          aria-label={`Expand Linear (${docs.length})`}
          onClick={toggleCollapsed}
          className="mt-1 text-xs font-semibold text-muted-foreground transition-colors hover:text-foreground"
        >
          {docs.length}
        </button>
      ) : (
        <ul className="min-h-0 space-y-2 overflow-y-auto">
          {/* Keep the shared oldest-first query cache intact. */}
          {docs
            .slice()
            .reverse()
            .map((doc) => (
              <RailLinearRow key={doc.url} doc={doc} />
            ))}
        </ul>
      )}
    </aside>
  );
}

/** One Linear ref in the fullscreen rail: a kind dot + identifier (truncated so
 *  it never steals width from the terminal). */
function RailLinearRow({ doc }: { doc: SessionLinearDoc }) {
  return (
    <li className="text-xs" data-testid="rail-linear-row">
      <a
        href={safeHref(doc.url)}
        target="_blank"
        rel="noopener noreferrer"
        title={`${doc.kind} · ${doc.identifier} · ${doc.workspace}`}
        className="flex min-w-0 items-center gap-1.5 font-medium text-foreground hover:text-primary hover:underline"
      >
        <LinearKindDot kind={doc.kind} />
        <span className="truncate">{doc.identifier}</span>
      </a>
    </li>
  );
}

/** Tiny colored dot encoding a Linear ref's kind (document / issue / project),
 *  matching the {@link LinearDocsCard} badge palette. */
function LinearKindDot({ kind }: { kind: string }) {
  const cls =
    kind === "document"
      ? "bg-blue-400"
      : kind === "issue"
        ? "bg-amber-400"
        : kind === "project"
          ? "bg-purple-400"
          : "bg-muted-foreground/50";
  return (
    <span
      aria-label={kind}
      title={kind}
      className={`inline-block h-2 w-2 shrink-0 rounded-full ${cls}`}
    />
  );
}

/**
 * The visible comments for one PR, most-recent first. Two classes of comment are
 * dropped as non-actionable, matching the PR-activity panel:
 *  - **resolved inline review comments** — the thread's been addressed;
 *  - **CodeRabbit rollups** — its review summaries ("Actionable comments posted:
 *    N") and issue/status posts, which have no resolution state and so would
 *    otherwise linger in the rail even after every inline finding is resolved
 *    (see {@link isCodeRabbitRollup}). CodeRabbit's *unresolved inline findings*
 *    are kept — they carry the real, actionable signal.
 * Server order is `created_at` ascending, so we reverse for "latest first".
 */
function commentsForPr(
  all: PrComment[] | undefined,
  pr: SessionPr,
): PrComment[] {
  if (!all) return [];
  return all
    .filter(
      (c) =>
        c.owner === pr.owner &&
        c.repo === pr.repo &&
        c.number === pr.number &&
        !(c.kind === "review_comment" && c.resolved === true) &&
        !isCodeRabbitRollup(c),
    )
    .slice()
    .reverse();
}

/** First non-empty line of a comment body, whitespace collapsed (so multi-line
 *  / markdown bodies stay a single truncatable line). */
function firstLine(body: string): string {
  return body.replace(/\s+/g, " ").trim();
}

/** Tiny colored dot encoding PR state (merged / draft / open / closed). */
function PrStateDot({ status }: { status: SessionPr["status"] }) {
  // `state` is gh's raw value (`OPEN`/`CLOSED`/`MERGED`) — normalize case.
  const state = status?.state?.toUpperCase();
  const { cls, label } = !status
    ? { cls: "bg-muted-foreground/50", label: "status unknown" }
    : status.merged
      ? { cls: "bg-purple-400", label: "merged" }
      : status.draft
        ? { cls: "bg-muted-foreground", label: "draft" }
        : state === "OPEN"
          ? { cls: "bg-green-400", label: "open" }
          : { cls: "bg-red-400", label: "closed" };
  return (
    <span
      aria-label={label}
      title={label}
      className={`inline-block h-2 w-2 shrink-0 rounded-full ${cls}`}
    />
  );
}
