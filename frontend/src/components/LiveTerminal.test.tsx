import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as React from "react";
import type { ReactNode } from "react";
import {
  act,
  fireEvent,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useLocation } from "react-router-dom";
import { renderWithProviders } from "@/test/utils";
import { api } from "@/lib/api";
import { HostControlError } from "@/lib/hostControl";
import { PrivateTerminalEnded } from "@/lib/privatePty";
import { useQueryClient, type QueryClient } from "@tanstack/react-query";
import { sessionPrsKey } from "@/components/ChatPanel";
import { sessionLinearDocsKey } from "@/components/LinearDocsCard";
import { writePreferredFontSize } from "@/lib/terminalFontSize";

vi.mock("@/lib/useLiveStream", () => ({
  useLiveStream: vi.fn(() => ({
    messages: [
      {
        uuid: "diagram-answer",
        role: "assistant",
        kind: "text",
        blocks: [
          { type: "text", text: "```mermaid\nflowchart LR\n A --> B\n```" },
        ],
      },
    ],
    outline: { count: 1, lastRole: "assistant" },
    status: { kind: "streaming" },
  })),
}));
vi.mock("@/lib/mermaid", async () => ({
  ...(await vi.importActual("@/lib/mermaid")),
  renderMermaidSvg: vi.fn(async () => '<svg viewBox="0 0 2400 400"></svg>'),
}));

// LiveTerminal reads `?fullscreen=1` via useSearchParams and (in fullscreen)
// renders LiveTabs, which needs the QueryClient + Toast providers too. Shadow
// `render` with the shared provider wrapper (initial entry "/" ⇒ windowed by
// default; the fullscreen toggle drives the URL itself). Pass a `route` with
// `?fullscreen=1` to simulate landing already in fullscreen.
function render(
  ui: ReactNode,
  {
    route = "/",
  }: { route?: string | { pathname: string; state: unknown } } = {},
) {
  return renderWithProviders(
    <>
      {ui}
      <LocationProbe />
    </>,
    { route },
  );
}

function LocationProbe() {
  return (
    <span hidden data-testid="location-search">
      {useLocation().search}
    </span>
  );
}

/** Desktop has a Display menu; compact screens fold it into Actions. */
async function openDisplayMenu() {
  const trigger =
    screen.queryByRole("button", { name: "Display" }) ??
    screen.getByRole("button", { name: "Actions" });
  await userEvent.setup().click(trigger);
}

/** Fullscreen lives in the URL (`?fullscreen=1`). */
function inFullscreen() {
  return screen.getByTestId("location-search").textContent === "?fullscreen=1";
}

async function toggleFullscreen() {
  await openDisplayMenu();
  await userEvent
    .setup()
    .click(
      await screen.findByRole("menuitem", { name: /^(exit )?fullscreen$/i }),
    );
}

/** Reads the Fullscreen item's current label, then closes the menu. */
async function fullscreenLabel() {
  await openDisplayMenu();
  const item = await screen.findByRole("menuitem", {
    name: /^(exit )?fullscreen$/i,
  });
  const label = item.textContent;
  await userEvent.setup().keyboard("{Escape}");
  return label;
}

const ZOOM_ITEM = /^(zoom to fit|reset zoom)$/i;

/** One item that reads "Zoom to fit" when zoom is off and "Reset zoom" when on. */
async function zoomItem() {
  if (!screen.queryByRole("menuitem", { name: ZOOM_ITEM })) {
    await openDisplayMenu();
  }
  return screen.getByRole("menuitem", { name: ZOOM_ITEM });
}

async function zoomLabel() {
  return (await zoomItem()).textContent;
}

async function toggleZoom() {
  await userEvent.setup().click(await zoomItem());
}

// ── Mock xterm — it touches canvas / DOM APIs not present in jsdom ─────────
const mockWrite = vi.fn();
const mockPaste = vi.fn();
const mockResize = vi.fn();
const mockReset = vi.fn();
const mockOpen = vi.fn();
const mockDispose = vi.fn();
const mockLoadAddon = vi.fn();
const mockFocus = vi.fn();
const mockScrollToBottom = vi.fn();
const mockScrollLines = vi.fn();
const mockScrollToLine = vi.fn();
const mockRefresh = vi.fn();
const mockOnDataDispose = vi.fn();
// Mutable xterm buffer position the `isPinnedToBottom` guard reads. Pinned when
// viewportY >= baseY (the default); a test sets viewportY < baseY to simulate the
// viewer having scrolled UP into history.
let bufferState = { viewportY: 0, baseY: 0 };
// Which xterm buffer is active. Touch scrolling branches on it: the normal
// buffer has local scrollback to move, an alternate-screen TUI (OpenCode) owns
// its own history and has to be sent a mouse report instead.
let bufferType = "normal";
// Capture the latest term.onData callback so tests can drive keystrokes.
let onDataCb: ((data: string) => void) | null = null;
// Capture the custom key-event handler so tests can drive raw key events
// (Shift+Enter, which never reaches onData because xterm folds it into a CR).
let customKeyHandler: ((e: KeyboardEvent) => boolean) | null = null;
// Capture the custom wheel handler so tests can drive the synchronous
// wheel-handler -> onData path xterm uses for TUI mouse input.
let customWheelHandler: ((e: WheelEvent) => boolean) | null = null;
// Selection + buffer lines the Slack-copy handler reads. Default: no selection
// (so the copy handler is a no-op, like the other tests expect). A copy test
// sets these to drive serialization.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
let selectionState: any = undefined;
// eslint-disable-next-line @typescript-eslint/no-explicit-any
let lineState: any[] = [];
// The latest xterm `onRender` listener (drives the diagram-marker scan).
let renderCb: (() => void) | null = null;
let autoCompleteWrites = true;
let pendingWriteCallbacks: Array<() => void> = [];

vi.mock("@xterm/xterm", () => ({
  Terminal: vi.fn().mockImplementation(function () {
    return {
      loadAddon: mockLoadAddon,
      open: mockOpen,
      paste: mockPaste,
      // Record the call (for assertions) AND invoke the write-complete callback so
      // the post-write scroll logic (`isPinnedToBottom` → `scrollToBottom`) runs.
      write: (data: unknown, cb?: () => void) => {
        mockWrite(data, cb);
        if (!cb) return;
        if (autoCompleteWrites) cb();
        else pendingWriteCallbacks.push(cb);
      },
      resize: mockResize,
      reset: mockReset,
      dispose: mockDispose,
      focus: mockFocus,
      refresh: mockRefresh,
      scrollToBottom: mockScrollToBottom,
      scrollLines: mockScrollLines,
      onScroll: () => ({ dispose: vi.fn() }),
      onRender: (cb: () => void) => {
        renderCb = cb;
        return { dispose: vi.fn() };
      },
      scrollToLine: mockScrollToLine,
      getSelectionPosition: () => selectionState,
      get buffer() {
        return {
          active: {
            ...bufferState,
            type: bufferType,
            getLine: (y: number) => lineState[y],
          },
        };
      },
      options: { disableStdin: true, fontSize: 13 },
      attachCustomKeyEventHandler: (cb: (e: KeyboardEvent) => boolean) => {
        customKeyHandler = cb;
      },
      attachCustomWheelEventHandler: (cb: (e: WheelEvent) => boolean) => {
        customWheelHandler = cb;
      },
      onData: (cb: (data: string) => void) => {
        onDataCb = cb;
        return { dispose: mockOnDataDispose };
      },
      // xterm exposes cols/rows on the instance after resize; start at 0 so
      // the first frame always triggers a resize (cols and rows will differ).
      cols: 0,
      rows: 0,
    };
  }),
}));

import { Terminal as XtermTerminal } from "@xterm/xterm";
import { WebglAddon } from "@xterm/addon-webgl";

const { mockWebglDispose, mockWebglOnContextLoss } = vi.hoisted(() => ({
  mockWebglDispose: vi.fn(),
  mockWebglOnContextLoss: vi.fn(),
}));
vi.mock("@xterm/addon-webgl", () => ({
  WebglAddon: vi.fn().mockImplementation(function () {
    return {
      dispose: mockWebglDispose,
      onContextLoss: mockWebglOnContextLoss,
    };
  }),
}));

const {
  mockSerialize,
  mockLoadTerminalSnapshot,
  mockSaveTerminalSnapshot,
  mockDeleteTerminalSnapshot,
} = vi.hoisted(() => ({
  mockSerialize: vi.fn(() => "serialized terminal"),
  mockLoadTerminalSnapshot: vi.fn().mockResolvedValue(null),
  mockSaveTerminalSnapshot: vi.fn().mockResolvedValue(undefined),
  mockDeleteTerminalSnapshot: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("@xterm/addon-serialize", () => ({
  SerializeAddon: vi.fn().mockImplementation(function () {
    return { serialize: mockSerialize, dispose: vi.fn() };
  }),
}));
vi.mock("@/lib/terminalCache", () => ({
  loadTerminalSnapshot: mockLoadTerminalSnapshot,
  saveTerminalSnapshot: mockSaveTerminalSnapshot,
  deleteTerminalSnapshot: mockDeleteTerminalSnapshot,
}));

// Capture the live-PTY controller's `sendInput` / `close` so capture-mode tests
// can assert on the keystroke payload. `vi.hoisted` so the mock fns exist before
// the hoisted vi.mock factories run.
const {
  mockSendInput,
  mockSocketSendInput,
  mockCtrlClose,
  mockRepaintLive,
  mockResizeLive,
  mockPasteImage,
  mockSendFiles,
  mockUnlockAgain,
  mockListSessionLinearDocs,
  mockListSessionPrs,
  mockPrivateLinearDocs,
  mockPrivatePrCacheRead,
  mockListPrComments,
} = vi.hoisted(() => ({
  mockSendFiles: vi.fn().mockResolvedValue(undefined),
  mockUnlockAgain: vi.fn().mockResolvedValue(undefined),
  // Keystrokes now go through the signed input channel (mocked below); the
  // terminal socket's own `sendInput` must never be used for input.
  mockSendInput: vi.fn(),
  mockSocketSendInput: vi.fn(),
  mockCtrlClose: vi.fn(),
  mockRepaintLive: vi.fn().mockResolvedValue({ delivered: true }),
  mockResizeLive: vi.fn().mockResolvedValue({ delivered: true }),
  mockPasteImage: vi.fn().mockResolvedValue(undefined),
  // The fullscreen Linear rail reads this; default empty so it renders nothing
  // (every other test). The rail-specific test overrides it per-case.
  mockListSessionLinearDocs: vi.fn().mockResolvedValue([]),
  mockPrivateLinearDocs: vi.fn().mockResolvedValue([]),
  mockPrivatePrCacheRead: vi.fn().mockResolvedValue({ prs: [], comments: [] }),
  // The fullscreen PR rail reads these; default empty so it renders nothing
  // (every other test). The PR-rail test overrides them per-case.
  mockListSessionPrs: vi.fn().mockResolvedValue([]),
  mockListPrComments: vi.fn().mockResolvedValue([]),
}));
vi.mock("@/lib/api", async () => {
  const actual = await vi.importActual<typeof import("@/lib/api")>("@/lib/api");
  return {
    ...actual,
    api: {
      ...actual.api,
      listSessionLinearDocs: mockListSessionLinearDocs,
      listSessionPrs: mockListSessionPrs,
      listPrComments: mockListPrComments,
      listPrCommentPreviews: mockListPrComments,
      // The toolbar's new-session dialog reads the default agent.
      getUserSettings: vi.fn().mockResolvedValue({
        container_paths: [],
        default_agent: "claude",
      }),
      liveSessions: vi.fn().mockResolvedValue([]),
      listSessions: vi.fn().mockResolvedValue([]),
    },
  };
});

// The signed host-control layer (its own tests cover signing, grants and
// verification). Here: keystrokes arrive as base64 on `mockSendInput`, and the
// terminal size/repaint requests go through the host, not the server API.
vi.mock("@/lib/hostControl", async () => {
  const actual =
    await vi.importActual<typeof import("@/lib/hostControl")>(
      "@/lib/hostControl",
    );
  const utf8b64 = (text: string) => {
    let binary = "";
    for (const byte of new TextEncoder().encode(text))
      binary += String.fromCharCode(byte);
    return btoa(binary);
  };
  return {
    ...actual,
    unlockAgain: (...args: unknown[]) => mockUnlockAgain(...args),
    createInputChannel: () => ({
      send: (text: string) => mockSendInput(utf8b64(text)),
      close: vi.fn(),
    }),
    hostControl: {
      ...actual.hostControl,
      // The signed path has no `{delivered:false}`: a host that cannot act
      // rejects, and the component reports that.
      resizeSession: async (...args: unknown[]) => {
        const result = await mockResizeLive(...args.slice(0, 3));
        if (result?.delivered === false)
          throw new Error("No CLI host attached");
      },
      repaintSession: (...args: unknown[]) => mockRepaintLive(...args),
      pasteImage: (...args: unknown[]) => mockPasteImage(...args),
      sendFiles: (...args: unknown[]) => mockSendFiles(...args),
      // Private metadata reads: only Linear lists are exercised here; they
      // are served from the same fixture mock as the old central list.
      privateRead: async (
        _hostId: string,
        read: (channel: {
          operation: (
            op: string,
            session: string,
            args: unknown,
          ) => Promise<unknown>;
        }) => Promise<unknown>,
      ) =>
        read({
          operation: async (op: string, session: string, _args: unknown) => {
            if (op === "linear.list") {
              return {
                status: "executed",
                body: {
                  kind: "linear",
                  docs: await mockPrivateLinearDocs(session),
                  next: null,
                },
              };
            }
            throw new Error(`unmocked private operation ${op}`);
          },
        }),
    },
  };
});

vi.mock("@/lib/privatePrCache", () => ({
  readPrivatePrCache: (...args: unknown[]) =>
    mockPrivatePrCacheRead(...(args as [string, string])),
}));

// Private transcript reads (terminal diagrams): the same fixture transcript
// the central useLiveStream mock serves.
vi.mock("@/lib/privateChannel", async () => ({
  ...(await vi.importActual<typeof import("@/lib/privateChannel")>(
    "@/lib/privateChannel",
  )),
  readHistory: async (
    _channel: unknown,
    _sessionId: string,
    { onPage }: { onPage: (page: { message: unknown }[]) => void },
  ) => {
    onPage([
      {
        message: {
          uuid: "diagram-answer",
          role: "assistant",
          kind: "text",
          blocks: [
            { type: "text", text: "```mermaid\nflowchart LR\n A --> B\n```" },
          ],
        },
      },
    ]);
  },
}));

const mockFit = vi.fn();
vi.mock("@xterm/addon-fit", () => ({
  FitAddon: vi.fn().mockImplementation(function () {
    return { fit: mockFit };
  }),
}));

// ── Mock openPrivatePty — capture the handlers so tests can drive frames ────
import type {
  LivePtyHandlers,
  LivePtyController,
  LivePtyEnd,
} from "@/lib/privatePty";

let capturedHandlers: LivePtyHandlers | null = null;
let capturedSignal: AbortSignal | null = null;

/** Build a controller sharing the hoisted `sendInput`/`close` spies, whose
 *  `closed` promise settles however the test wants. */
function makeController(closed: Promise<LivePtyEnd>): LivePtyController {
  return { sendInput: mockSocketSendInput, close: mockCtrlClose, closed };
}

/** A never-settling `closed` (the default: a live stream that stays connected).
 *  `<never>` so it type-checks as `Promise<LivePtyEnd>`. */
const neverClosed = () => new Promise<never>(() => {});

// The private-channel source has the same controller shape; its own tests
// cover the channel mapping. Here: which source the terminal uses, and how.
const mockOpenPrivatePty = vi.fn(
  (
    _host: string,
    _id: string,
    handlers: LivePtyHandlers,
    signal?: AbortSignal,
  ): LivePtyController => {
    capturedHandlers = handlers;
    capturedSignal = signal ?? null;
    // Private subscribe success is the replay boundary — see `openPrivatePty`.
    handlers.onOpen?.();
    handlers.onReplayComplete?.();
    return makeController(neverClosed());
  },
);
vi.mock("@/lib/privatePty", async () => ({
  ...(await vi.importActual<typeof import("@/lib/privatePty")>(
    "@/lib/privatePty",
  )),
  openPrivatePty: (...args: Parameters<typeof mockOpenPrivatePty>) =>
    mockOpenPrivatePty(...args),
}));

import {
  GRID_CONFIRM_TIMEOUT_MS,
  LiveTerminal,
  PTY_RECONNECT_BACKOFF_MS,
} from "./LiveTerminal";

/** The next openPrivatePty subscribes but never completes its replay: the
 *  terminal stays pre-live until the test drives frames/onReplayComplete. */
function holdReplay() {
  mockOpenPrivatePty.mockImplementationOnce((_host, _id, handlers, signal) => {
    capturedHandlers = handlers;
    capturedSignal = signal ?? null;
    handlers.onOpen?.();
    return makeController(neverClosed());
  });
}

// ── Helpers ────────────────────────────────────────────────────────────────

function makeBytes(values: number[]): Uint8Array {
  return new Uint8Array(values);
}

// Desktop arms capture by clicking the terminal, and shows a "Keyboard captured"
// status (the only way to release) while it is on. Nothing is shown when idle.
const armCapture = () =>
  act(() => {
    fireEvent.click(screen.getByTestId("live-terminal-screen"));
  });
const releaseControl = () =>
  screen.queryByRole("button", { name: /release keyboard/i });
const isCapturing = () => releaseControl() !== null;

/**
 * Point the mocked terminal at the rendered grid element and give the grid and
 * the terminal box the pixel geometry jsdom never lays out. Defaults describe an
 * 80x24 grid drawn at the 13px base font. Returns the elements so a test can
 * spy on them.
 */
function measureTerminal({
  viewportWidth,
  viewportHeight,
  renderedWidth = 640,
  renderedHeight = 384,
  // Box scroll height. Defaults to the viewport height — a fitted grid, with
  // nothing left to pan. Raise it to describe a grid taller than the screen.
  contentHeight = viewportHeight,
}: {
  viewportWidth: number;
  viewportHeight: number;
  renderedWidth?: number;
  renderedHeight?: number;
  contentHeight?: number;
}) {
  const term = vi.mocked(XtermTerminal).mock.results.at(-1)?.value as {
    element?: HTMLElement;
  };
  const grid = screen.getByTestId("live-terminal-grid");
  const box = screen.getByTestId("live-terminal-screen");
  const define = (el: object, prop: string, value: number) =>
    Object.defineProperty(el, prop, { configurable: true, value });
  Object.defineProperty(term, "element", { configurable: true, value: grid });
  define(grid, "offsetWidth", renderedWidth);
  define(grid, "offsetHeight", renderedHeight);
  define(box, "clientWidth", viewportWidth);
  define(box, "clientHeight", viewportHeight);
  define(box, "scrollHeight", contentHeight);
  // jsdom lays nothing out, so `scrollTop` is a hard 0 there; back it with a
  // real slot so a test can observe panning. Clamp like a browser does — the
  // pan handler reads back what the box actually took.
  let scrollTop = 0;
  Object.defineProperty(box, "scrollTop", {
    configurable: true,
    get: () => scrollTop,
    set: (v: number) => {
      scrollTop = Math.max(0, Math.min(v, contentHeight - viewportHeight));
    },
  });
  return { term, grid, box };
}

/** Deterministic animation clock for touch-momentum behavior tests. */
function installAnimationFrameClock() {
  let now = 0;
  let nextId = 1;
  const frames = new Map<number, FrameRequestCallback>();
  const canceled = new Set<number>();
  const nowSpy = vi.spyOn(performance, "now").mockImplementation(() => now);
  const requestSpy = vi
    .spyOn(window, "requestAnimationFrame")
    .mockImplementation((callback) => {
      const id = nextId++;
      frames.set(id, callback);
      return id;
    });
  const cancelSpy = vi
    .spyOn(window, "cancelAnimationFrame")
    .mockImplementation((id) => {
      // Before a frame starts, removing the queued callback is enough. While
      // `advance` is dispatching a snapshot, remember the id so an earlier
      // callback can still cancel a later callback selected for that frame.
      if (!frames.delete(id)) canceled.add(id);
    });

  return {
    advance(ms = 16) {
      now += ms;
      const pending = [...frames.entries()];
      frames.clear();
      act(() => {
        for (const [id, callback] of pending) {
          if (canceled.delete(id)) continue;
          callback(now);
        }
      });
    },
    get pending() {
      return frames.size;
    },
    restore() {
      nowSpy.mockRestore();
      requestSpy.mockRestore();
      cancelSpy.mockRestore();
    },
  };
}

// ── Tests ──────────────────────────────────────────────────────────────────

it("the animation clock honors cancellation during the current frame", () => {
  const clock = installAnimationFrameClock();
  try {
    let canceledCallbackRan = false;
    let laterFrame = 0;
    requestAnimationFrame(() => cancelAnimationFrame(laterFrame));
    laterFrame = requestAnimationFrame(() => {
      canceledCallbackRan = true;
    });

    clock.advance();

    expect(canceledCallbackRan).toBe(false);
  } finally {
    clock.restore();
  }
});

beforeEach(() => {
  vi.clearAllMocks();
  // clearAllMocks wipes call history but keeps implementations, so restore the
  // empty-rail defaults a per-case override may have left behind.
  mockListSessionLinearDocs.mockResolvedValue([]);
  mockListSessionPrs.mockResolvedValue([]);
  mockListPrComments.mockResolvedValue([]);
  mockLoadTerminalSnapshot.mockResolvedValue(null);
  mockSerialize.mockReturnValue("serialized terminal");
  capturedHandlers = null;
  capturedSignal = null;
  onDataCb = null;
  customKeyHandler = null;
  customWheelHandler = null;
  bufferState = { viewportY: 0, baseY: 0 }; // pinned to bottom by default
  bufferType = "normal";
  selectionState = undefined;
  lineState = [];
  renderCb = null;
  autoCompleteWrites = true;
  pendingWriteCallbacks = [];
  try {
    localStorage.removeItem("arrakisTermCodeColors");
    localStorage.removeItem("arrakisTerminalFontSize");
  } catch {
    /* jsdom always has localStorage; guard anyway */
  }
});

describe("LiveTerminal", () => {
  it("re-surfaces the Settings gear as the RIGHTMOST control in the fullscreen overlay", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-gear",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-gear"
        viewerIsAuthor
      />,
      {
        route: "/?fullscreen=1",
      },
    );
    // The gear must stay reachable in fullscreen — the overlay (z-60, fixed
    // inset-0) sits above the sticky AppShell header, so any Settings control
    // living only in the shell would be invisible here. It must also sit at
    // the far right of the toolbar, AFTER the Display / Actions menus — not
    // next to the bell at the left.
    const gear = screen.getByRole("link", { name: "Settings" });
    expect(gear).toHaveAttribute("href", "/settings");

    // Walk every interactive toolbar control in document order; the gear
    // must be last. The bell, `New session`, and both the Display and
    // Actions menus must all appear before it.
    const toolbar = screen.getByTestId("session-compact-row");
    const controls = Array.from(
      toolbar.querySelectorAll<HTMLElement>(
        'a[href], button:not([aria-hidden="true"])',
      ),
    );
    expect(controls[controls.length - 1]).toBe(gear);
  });

  it("does not add a second Settings gear when windowed (the AppShell one already shows)", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-windowed",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-windowed"
        viewerIsAuthor
      />,
    );
    expect(screen.queryByRole("link", { name: "Settings" })).toBeNull();
  });

  it("opens terminal diagrams from the Display menu in fullscreen and keeps popup keys out of the PTY", async () => {
    const user = userEvent.setup();
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "diagram-live",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="diagram-live"
        viewerIsAuthor
      />,
      {
        route: "/?fullscreen=1",
      },
    );
    expect(isCapturing()).toBe(true);
    // No standalone Diagrams button in the toolbar — it must live in the menu.
    expect(
      screen.queryByRole("button", { name: /terminal diagrams/i }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /^diagrams$/i }),
    ).not.toBeInTheDocument();
    await openDisplayMenu();
    await user.click(
      await screen.findByRole("menuitem", { name: /^diagrams$/i }),
    );
    expect(
      await screen.findByTitle("Expanded Mermaid diagram"),
    ).toBeInTheDocument();
    mockSendInput.mockClear();
    await user.keyboard("{ArrowRight}{Escape}");
    expect(mockSendInput).not.toHaveBeenCalled();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.getByTestId("location-search")).toHaveTextContent(
      "fullscreen=1",
    );
  });

  // A buffer line that answers per cell like xterm: a CJK character fills two
  // cells, its continuation cell holding no characters.
  const cellLine = (text: string) => {
    const cells = Array.from(text).flatMap((char) =>
      /[\u3000-\u9fff]/.test(char) ? [char, ""] : [char],
    );
    return {
      translateToString: () => text,
      getCell: (col: number) => ({ getChars: () => cells[col] ?? "" }),
    };
  };
  const diagram = [
    "Here is the flow:",
    "┌──────┐",
    "│ API  │──┐",
    "└──────┘  │",
    "┌──────┐  │",
    "│ 数据 │◄─┘",
    "└──────┘",
    "Done.",
  ];
  // Render a live terminal whose 40x`rows` grid paints 10px cells.
  const renderDiagramTerminal = (rows: number) => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "diagram-live",
            agent: "codex",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="diagram-live"
        viewerIsAuthor
      />,
    );
    const term = vi.mocked(XtermTerminal).mock.results.at(-1)?.value as {
      cols: number;
      rows: number;
      element?: HTMLElement;
    };
    term.cols = 40;
    term.rows = rows;
    term.element = document.createElement("div");
    const grid = term.element.appendChild(document.createElement("div"));
    grid.className = "xterm-screen";
    grid.getBoundingClientRect = () =>
      ({ left: 0, top: 0, width: 400, height: rows * 10 }) as DOMRect;
    act(() => renderCb?.());
  };

  it("marks a diagram drawn in the terminal and opens the viewer from it", async () => {
    const width = vi
      .spyOn(HTMLElement.prototype, "clientWidth", "get")
      .mockReturnValue(1000);
    try {
      lineState = diagram.map(cellLine);
      renderDiagramTerminal(diagram.length);

      const marker = await screen.findByRole("button", {
        name: "View diagram",
      });
      // Right of the widest row's last glyph (cell 11, after the wide label),
      // level with the first node.
      expect(marker).toHaveStyle({ left: "120px", top: "10px" });
      await userEvent.setup().click(marker);

      expect(
        await screen.findByTitle("Expanded Mermaid diagram"),
      ).toBeInTheDocument();
    } finally {
      width.mockRestore();
    }
  });

  it("keeps the marker while the diagram's first node is scrolled above the viewport", async () => {
    const width = vi
      .spyOn(HTMLElement.prototype, "clientWidth", "get")
      .mockReturnValue(1000);
    try {
      lineState = diagram.map(cellLine);
      bufferState = { viewportY: 2, baseY: 2 };
      renderDiagramTerminal(diagram.length - 2);

      expect(
        await screen.findByRole("button", { name: "View diagram" }),
      ).toHaveStyle({ top: "0px" });
    } finally {
      width.mockRestore();
    }
  });

  it("does not mark terminal output without a diagram", async () => {
    lineState = ["┌──────┬──────┐", "│ a    │ b    │", "└──────┴──────┘"].map(
      cellLine,
    );
    renderDiagramTerminal(lineState.length);
    await act(() => new Promise((resolve) => requestAnimationFrame(resolve)));

    expect(screen.queryByRole("button", { name: "View diagram" })).toBeNull();
  });

  it("offers terminal diagrams to a read-only viewer from the Display menu", async () => {
    const user = userEvent.setup();
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "diagram-read-only",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="diagram-read-only"
        viewerIsAuthor={false}
        viewerCanWrite={false}
      />,
    );
    await openDisplayMenu();
    await user.click(
      await screen.findByRole("menuitem", { name: /^diagrams$/i }),
    );
    expect(
      await screen.findByTitle("Expanded Mermaid diagram"),
    ).toBeInTheDocument();
    expect(isCapturing()).toBe(false);
  });

  it("drops the Diagrams menu item when the session is not active (nothing to show)", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-inactive",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-inactive"
        active={false}
      />,
    );
    await openDisplayMenu();
    // The rest of the Display items are still here…
    expect(
      await screen.findByRole("menuitem", {
        name: /^(zoom to fit|reset zoom)$/i,
      }),
    ).toBeInTheDocument();
    // …but Diagrams is omitted because an ended session has no stream to tap.
    expect(
      screen.queryByRole("menuitem", { name: /^diagrams$/i }),
    ).not.toBeInTheDocument();
  });

  it("opens the terminal into the container div on mount", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
      />,
    );
    // open() is called with a DOM element
    expect(mockOpen).toHaveBeenCalledTimes(1);
    expect(mockOpen).toHaveBeenCalledWith(expect.any(HTMLDivElement));
  });

  it("preserves cleared terminal screens in scrollback", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-scrollback",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-scrollback"
      />,
    );
    expect(vi.mocked(XtermTerminal)).toHaveBeenCalledWith(
      expect.objectContaining({
        scrollback: 5000,
        scrollOnEraseInDisplay: true,
      }),
    );
  });

  it("starts streaming for the given session id", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-42",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-42"
      />,
    );
    expect(mockOpenPrivatePty).toHaveBeenCalledWith(
      "host",
      "sess-42",
      expect.objectContaining({ onFrame: expect.any(Function) }),
      expect.any(AbortSignal),
    );
  });

  it("keeps the xterm buffer and PTY socket alive while inactive", async () => {
    let setActive: React.Dispatch<React.SetStateAction<boolean>> | undefined;
    function Harness() {
      const [active, updateActive] = React.useState(true);
      setActive = updateActive;
      return (
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-pooled",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-pooled"
          active={active}
        />
      );
    }

    render(<Harness />);
    const signal = capturedSignal;

    act(() => setActive?.(false));
    act(() => setActive?.(true));

    expect(mockOpenPrivatePty).toHaveBeenCalledTimes(1);
    expect(mockDispose).not.toHaveBeenCalled();
    expect(signal?.aborted).toBe(false);
  });

  it.each([800, 240])(
    "keeps fullscreen position %i when capture refocuses after a session switch",
    (position) => {
      let setActive: React.Dispatch<React.SetStateAction<boolean>>;
      function Harness() {
        const [active, updateActive] = React.useState(true);
        setActive = updateActive;
        return (
          <LiveTerminal
            privateHostId="host"
            privateSessions={[
              {
                id: "sess-scroll-switch",
                agent: "claude",
                cwd: "/work",
                title: null,
                messages: 1,
                last_activity: null,
                running: true,
              },
            ]}
            id="sess-scroll-switch"
            active={active}
            viewerIsAuthor
          />
        );
      }
      render(<Harness />, {
        route: "/sessions/sess-scroll-switch?fullscreen=1",
      });
      const { term, box } = measureTerminal({
        viewportWidth: 800,
        viewportHeight: 400,
        contentHeight: 1200,
      });
      // Model the browser revealing xterm's off-screen helper textarea on focus.
      Object.assign(term, {
        textarea: {
          focus: (options?: FocusOptions) => {
            if (!options?.preventScroll) box.scrollTop = 0;
          },
          blur: vi.fn(),
        },
      });
      box.scrollTop = position;
      act(() => setActive(false));
      act(() => setActive(true));
      expect(box.scrollTop).toBe(position);
    },
  );

  it("drops keyboard capture when a pooled terminal becomes inactive", async () => {
    const user = userEvent.setup();
    let setActive: React.Dispatch<React.SetStateAction<boolean>> | undefined;
    function Harness() {
      const [active, updateActive] = React.useState(true);
      setActive = updateActive;
      return (
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-pooled-input",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-pooled-input"
          active={active}
          viewerIsAuthor
        />
      );
    }

    render(<Harness />);
    await waitFor(() => expect(WebglAddon).toHaveBeenCalled());
    mockWebglDispose.mockClear();

    await user.click(screen.getByTestId("live-terminal-screen"));
    expect(isCapturing()).toBe(true);

    act(() => setActive?.(false));

    expect(isCapturing()).toBe(false);
    await waitFor(() => expect(mockOnDataDispose).toHaveBeenCalled());
    await waitFor(() => expect(mockWebglDispose).toHaveBeenCalledTimes(1));
  });

  it("writes bytes to the terminal without resizing when dims are unchanged", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
      />,
    );

    expect(capturedHandlers).not.toBeNull();
    const bytes = makeBytes([72, 101, 108, 108, 111]); // "Hello"

    // First frame: triggers resize (initial cols/rows are 0).
    capturedHandlers!.onFrame(80, 24, bytes);
    expect(mockResize).toHaveBeenCalledWith(80, 24);
    // write is called with (bytes, rescaleCallback).
    expect(mockWrite).toHaveBeenCalledWith(bytes, expect.any(Function));

    // Same dims — must NOT resize again.
    const moreBytes = makeBytes([32, 87, 111]);
    capturedHandlers!.onFrame(80, 24, moreBytes);
    expect(mockResize).toHaveBeenCalledTimes(1); // still just the one call
    expect(mockWrite).toHaveBeenCalledWith(moreBytes, expect.any(Function));
  });

  it("shows a loading spinner until replay and queued terminal writes finish", () => {
    autoCompleteWrites = false;
    holdReplay();
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-loading",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-loading"
      />,
    );
    expect(screen.getByText("Loading terminal…")).toBeVisible();
    act(() => {
      capturedHandlers!.onFrame(80, 24, makeBytes([65]));
      capturedHandlers!.onReplayComplete?.();
    });
    expect(screen.getByText("Loading terminal…")).toBeVisible();
    act(() => pendingWriteCallbacks.shift()?.());
    expect(screen.queryByText("Loading terminal…")).not.toBeInTheDocument();
  });

  it("auto-scrolls to the bottom on new output when pinned to the bottom", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
      />,
    );
    expect(capturedHandlers).not.toBeNull();
    // Pinned (viewportY >= baseY, the default) → new output tails to the bottom.
    act(() => capturedHandlers!.onFrame(80, 24, makeBytes([72, 105])));
    expect(mockScrollToBottom).toHaveBeenCalled();
  });

  it("does NOT yank to the bottom when the viewer has scrolled up", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
      />,
    );
    expect(capturedHandlers).not.toBeNull();
    // Viewer scrolled up into history: viewportY < baseY → leave their position.
    bufferState = { viewportY: 3, baseY: 120 };
    act(() => capturedHandlers!.onFrame(80, 24, makeBytes([72, 105])));
    expect(mockScrollToBottom).not.toHaveBeenCalled();
    expect(
      screen.getByRole("button", { name: "Jump to live" }),
    ).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Jump to live" }));
    expect(mockScrollToBottom).toHaveBeenCalled();
    expect(
      screen.queryByRole("button", { name: "Jump to live" }),
    ).not.toBeInTheDocument();
  });

  it("scrolls the terminal into view once when it first goes live", () => {
    // jsdom doesn't implement scrollIntoView — define a spy to observe it.
    const spy = vi.fn();
    Element.prototype.scrollIntoView = spy;
    holdReplay();

    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
      />,
    );
    expect(spy).not.toHaveBeenCalled();

    // First frame flips conn → "live", which triggers the scroll-into-view.
    act(() => capturedHandlers!.onFrame(80, 24, makeBytes([72, 105])));
    expect(spy).toHaveBeenCalledTimes(1);

    // Subsequent frames must NOT re-scroll the page.
    act(() => capturedHandlers!.onFrame(80, 24, makeBytes([33])));
    expect(spy).toHaveBeenCalledTimes(1);
  });

  it("resizes BEFORE writing when dims change across frames", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
      />,
    );
    expect(capturedHandlers).not.toBeNull();

    const bytes1 = makeBytes([1, 2]);
    capturedHandlers!.onFrame(80, 24, bytes1);

    const resizeOrder: string[] = [];
    mockResize.mockImplementation(() => resizeOrder.push("resize"));
    mockWrite.mockImplementation(() => resizeOrder.push("write"));

    // Dimension change: 80×24 → 132×43
    const bytes2 = makeBytes([3, 4]);
    capturedHandlers!.onFrame(132, 43, bytes2);

    expect(mockResize).toHaveBeenLastCalledWith(132, 43);
    // resize must have been recorded before write in this call
    expect(resizeOrder[0]).toBe("resize");
    expect(resizeOrder[1]).toBe("write");
  });

  it("does not resize for a later frame until xterm finishes the prior write", () => {
    autoCompleteWrites = false;
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-serialized",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-serialized"
      />,
    );

    act(() => {
      capturedHandlers!.onFrame(80, 24, makeBytes([65]));
      capturedHandlers!.onFrame(40, 24, makeBytes([66]));
    });

    expect(mockResize).toHaveBeenCalledTimes(1);
    expect(mockResize).toHaveBeenLastCalledWith(80, 24);
    expect(mockWrite).toHaveBeenCalledTimes(1);

    act(() => pendingWriteCallbacks.shift()?.());

    expect(mockResize).toHaveBeenCalledTimes(2);
    expect(mockResize).toHaveBeenLastCalledWith(40, 24);
    expect(mockWrite).toHaveBeenCalledTimes(2);
  });

  it("ignores a pending xterm write callback after unmount", () => {
    autoCompleteWrites = false;
    const { unmount } = render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-late-write",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-late-write"
      />,
    );
    act(() => capturedHandlers!.onFrame(80, 24, makeBytes([65])));
    expect(pendingWriteCallbacks).toHaveLength(1);

    unmount();
    act(() => pendingWriteCallbacks.shift()?.());

    expect(mockScrollToBottom).not.toHaveBeenCalled();
    expect(mockSaveTerminalSnapshot).not.toHaveBeenCalled();
  });

  it("disposes the terminal and aborts the stream on unmount", () => {
    const { unmount } = render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
      />,
    );
    expect(capturedSignal?.aborted).toBe(false);
    unmount();
    expect(mockDispose).toHaveBeenCalledTimes(1);
    expect(capturedSignal?.aborted).toBe(true);
  });

  it("calls onEnded when the PTY stream ends with eof", async () => {
    mockOpenPrivatePty.mockImplementationOnce((_host, _id, handlers) => {
      handlers.onOpen?.();
      return makeController(Promise.resolve({ reason: "eof" }));
    });
    const onEnded = vi.fn();
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-eof",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-eof"
        onEnded={onEnded}
      />,
    );
    // Let the resolved promise's .then run.
    await Promise.resolve();
    await Promise.resolve();
    expect(onEnded).toHaveBeenCalledTimes(1);
  });

  it("ends without reconnecting on a clean socket close", async () => {
    vi.useFakeTimers();
    try {
      const onEnded = vi.fn();
      // A clean close (`closed` resolves eof) means the host ended the stream
      // (the agent exited / channel closed) — terminal, so do not reconnect.
      mockOpenPrivatePty.mockImplementationOnce((_host, _id, handlers) => {
        handlers.onOpen?.();
        return makeController(Promise.resolve({ reason: "eof" }));
      });

      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-gone",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-gone"
          onEnded={onEnded}
        />,
      );
      await vi.advanceTimersByTimeAsync(PTY_RECONNECT_BACKOFF_MS + 50);

      expect(onEnded).toHaveBeenCalledTimes(1);
      // No reconnect attempt after a clean close.
      expect(mockOpenPrivatePty).toHaveBeenCalledTimes(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("repaints when the tab becomes visible again (heals an event-less WebGL buffer loss)", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-vis",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-vis"
      />,
    );
    // The mount already painted once; isolate the visibility-recovery repaint.
    mockRefresh.mockClear();
    act(() => {
      // jsdom's document.visibilityState defaults to "visible", so the handler
      // treats this as a return-to-foreground and repaints the current renderer.
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(mockRefresh).toHaveBeenCalled();
  });

  it("offers a fullscreen toggle in the Display menu that flips its label", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
      />,
    );
    expect(
      screen.queryByRole("button", { name: /fullscreen/i }),
    ).not.toBeInTheDocument();
    expect(await fullscreenLabel()).toBe("Fullscreen");
    await toggleFullscreen();
    expect(await fullscreenLabel()).toBe("Exit fullscreen");
  });

  it("fills a wide terminal without resizing the source and restores actual size", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-scale-toggle",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-scale-toggle"
      />,
    );
    const term = vi.mocked(XtermTerminal).mock.results.at(-1)?.value as {
      element?: HTMLElement;
      options: { fontSize: number };
    };
    const grid = screen.getByTestId("live-terminal-grid");
    const outer = grid.parentElement!;
    Object.defineProperty(term, "element", { configurable: true, value: grid });
    Object.defineProperty(grid, "offsetWidth", {
      configurable: true,
      get: () => (675 * term.options.fontSize) / 13,
    });
    Object.defineProperty(grid, "offsetHeight", {
      configurable: true,
      get: () => (300 * term.options.fontSize) / 13,
    });
    Object.defineProperty(outer, "clientWidth", {
      configurable: true,
      value: 1600,
    });

    await toggleZoom();
    await waitFor(() => expect(term.options.fontSize).toBeGreaterThan(28));
    expect(grid.offsetWidth).toBeLessThanOrEqual(1600);
    expect(mockResizeLive).not.toHaveBeenCalled();

    await toggleZoom();
    await waitFor(() => expect(term.options.fontSize).toBe(13));
    expect(await zoomLabel()).toBe("Zoom to fit");
  });

  it("renders at the preferred font size and follows a change to it", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-preferred-font",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-preferred-font"
      />,
    );
    const term = vi.mocked(XtermTerminal).mock.results.at(-1)?.value as {
      element?: HTMLElement;
      options: { fontSize: number };
    };
    const grid = screen.getByTestId("live-terminal-grid");
    const outer = grid.parentElement!;
    Object.defineProperty(term, "element", { configurable: true, value: grid });
    Object.defineProperty(grid, "offsetWidth", {
      configurable: true,
      get: () => (675 * term.options.fontSize) / 13,
    });
    Object.defineProperty(grid, "offsetHeight", {
      configurable: true,
      get: () => (300 * term.options.fontSize) / 13,
    });
    Object.defineProperty(outer, "clientWidth", {
      configurable: true,
      value: 1600,
    });

    act(() => writePreferredFontSize(18));
    await waitFor(() => expect(term.options.fontSize).toBe(18));

    act(() => writePreferredFontSize(null));
    await waitFor(() => expect(term.options.fontSize).toBe(13));
    expect(mockResizeLive).not.toHaveBeenCalled();
  });

  it("settles quantized font metrics and can grow after the viewport widens", async () => {
    const clock = installAnimationFrameClock();
    let notifyResize = () => {};
    vi.stubGlobal(
      "ResizeObserver",
      class {
        constructor(callback: () => void) {
          notifyResize = callback;
        }
        observe() {}
        disconnect() {}
      },
    );
    try {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-quantized-font",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-quantized-font"
        />,
      );
      const term = vi.mocked(XtermTerminal).mock.results.at(-1)?.value as {
        element?: HTMLElement;
        cols: number;
        rows: number;
        options: { fontSize: number };
      };
      term.cols = 80;
      term.rows = 24;
      const grid = screen.getByTestId("live-terminal-grid");
      const outer = grid.parentElement!;
      let availableWidth = 605;
      Object.defineProperty(term, "element", {
        configurable: true,
        value: grid,
      });
      // Xterm floors device-pixel character width before multiplying by cols.
      // Thus 13px has a 7px cell, while 14px jumps to an 8px cell; linear
      // prediction says 603px fits, but the actual next render is 640px wide.
      Object.defineProperty(grid, "offsetWidth", {
        configurable: true,
        get: () => Math.floor(term.options.fontSize * 0.6) * term.cols,
      });
      Object.defineProperty(grid, "offsetHeight", {
        configurable: true,
        get: () => term.options.fontSize * term.rows,
      });
      Object.defineProperty(outer, "clientWidth", {
        configurable: true,
        get: () => availableWidth,
      });
      const resizeTerminal = notifyResize;
      await toggleZoom();
      for (let frame = 0; frame < 12; frame++) clock.advance();
      expect(term.options.fontSize).toBe(13);
      expect(
        clock.pending,
        "font fitting must stop scheduling animation frames",
      ).toBe(0);
      availableWidth = 805;
      act(() => resizeTerminal());
      for (let frame = 0; frame < 60; frame++) clock.advance();
      expect(term.options.fontSize).toBeGreaterThan(13);
      expect(grid.offsetWidth).toBeLessThanOrEqual(availableWidth);
      expect(clock.pending).toBe(0);
      const widerFont = term.options.fontSize;
      term.cols = 40;
      act(() => resizeTerminal());
      for (let frame = 0; frame < 60; frame++) clock.advance();
      expect(term.options.fontSize).toBeGreaterThan(widerFont);
      expect(grid.offsetWidth).toBeLessThanOrEqual(availableWidth);
      expect(clock.pending).toBe(0);
    } finally {
      clock.restore();
      vi.unstubAllGlobals();
    }
  });

  it("fits the font to the cell metrics WebGL draws with, not the DOM renderer's", async () => {
    // Measured in a real browser: the font is chosen on the DOM renderer, then
    // WebGL draws the same font with narrower cells and nothing re-measured, so
    // the grid sat well short of the space. jsdom has no metrics; model WebGL
    // as 10% narrower than the DOM renderer.
    const clock = installAnimationFrameClock();
    try {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-webgl-fit",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-webgl-fit"
        />,
      );
      const term = vi.mocked(XtermTerminal).mock.results.at(-1)?.value as {
        element?: HTMLElement;
        cols: number;
        rows: number;
        options: { fontSize: number };
      };
      term.cols = 80;
      term.rows = 24;
      const grid = screen.getByTestId("live-terminal-grid");
      const outer = grid.parentElement!;
      const webglAttached = () =>
        vi.mocked(WebglAddon).mock.calls.length >
        mockWebglDispose.mock.calls.length;
      Object.defineProperty(term, "element", {
        configurable: true,
        value: grid,
      });
      Object.defineProperty(grid, "offsetWidth", {
        configurable: true,
        get: () => term.options.fontSize * 60 * (webglAttached() ? 0.9 : 1),
      });
      Object.defineProperty(grid, "offsetHeight", {
        configurable: true,
        get: () => term.options.fontSize * 24,
      });
      Object.defineProperty(outer, "clientWidth", {
        configurable: true,
        value: 1000,
      });
      vi.mocked(WebglAddon).mockClear();
      mockWebglDispose.mockClear();

      await toggleZoom();
      for (let frame = 0; frame < 80; frame++) clock.advance();

      // 18px is the largest font whose WebGL grid (972px) fits; sizing by DOM
      // metrics stops at 16px (a 864px WebGL grid).
      expect(term.options.fontSize).toBe(18);
      expect(webglAttached()).toBe(true);
      expect(grid.offsetWidth).toBeLessThanOrEqual(1000);
      expect(clock.pending, "font fitting must settle").toBe(0);
    } finally {
      clock.restore();
    }
  });

  it("stops resizing the font when its own height toggles the page scrollbar", () => {
    // Measured in a real browser: with Zoom to fit off, a 13px grid is a touch
    // too wide beside the page scrollbar, 12px is not tall enough to need one,
    // so the font flipped 12 <-> 13 every frame, forever.
    const clock = installAnimationFrameClock();
    let notifyResize = () => {};
    vi.stubGlobal(
      "ResizeObserver",
      class {
        constructor(callback: () => void) {
          notifyResize = callback;
        }
        observe() {}
        disconnect() {}
      },
    );
    const innerWidth = Object.getOwnPropertyDescriptor(window, "innerWidth")!;
    const scrollbar = 8;
    const viewportWidth = 1200;
    const viewportHeight = 863;
    const widthWithoutScrollbar = 783;
    // A scrollbar probe is an `overflow: scroll` box: it is 8px narrower inside.
    const offsetWidth = vi
      .spyOn(HTMLElement.prototype, "offsetWidth", "get")
      .mockImplementation(function (this: HTMLElement) {
        return this.style.overflow === "scroll" ? 100 : 0;
      });
    const clientWidth = vi
      .spyOn(HTMLElement.prototype, "clientWidth", "get")
      .mockImplementation(function (this: HTMLElement) {
        return this.style.overflow === "scroll" ? 100 - scrollbar : 0;
      });
    try {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-scrollbar-loop",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-scrollbar-loop"
        />,
      );
      const term = vi.mocked(XtermTerminal).mock.results.at(-1)?.value as {
        element?: HTMLElement;
        options: { fontSize: number };
      };
      const grid = screen.getByTestId("live-terminal-grid");
      const outer = grid.parentElement!;
      Object.defineProperty(term, "element", {
        configurable: true,
        value: grid,
      });
      const pageHeight = () => term.options.fontSize * 40 + 360;
      const pageScrolls = () => pageHeight() > viewportHeight;
      Object.defineProperty(window, "innerWidth", {
        configurable: true,
        value: viewportWidth,
      });
      Object.defineProperty(document.documentElement, "clientWidth", {
        configurable: true,
        get: () => viewportWidth - (pageScrolls() ? scrollbar : 0),
      });
      Object.defineProperty(grid, "offsetWidth", {
        configurable: true,
        get: () => term.options.fontSize * 60,
      });
      Object.defineProperty(grid, "offsetHeight", {
        configurable: true,
        get: () => term.options.fontSize * 40,
      });
      Object.defineProperty(outer, "clientWidth", {
        configurable: true,
        get: () => widthWithoutScrollbar - (pageScrolls() ? scrollbar : 0),
      });

      // The scrollbar width is measured once; a resize drops earlier tests'
      // cached value so this test's 8px scrollbar is measured.
      act(() => window.dispatchEvent(new Event("resize")));
      act(() => notifyResize());
      for (let frame = 0; frame < 20; frame++) clock.advance();

      expect(
        clock.pending,
        "font fitting must stop scheduling animation frames",
      ).toBe(0);
      const settled = term.options.fontSize;
      expect(grid.offsetWidth).toBeLessThanOrEqual(
        widthWithoutScrollbar - scrollbar,
      );
      for (let frame = 0; frame < 20; frame++) clock.advance();
      expect(term.options.fontSize).toBe(settled);
    } finally {
      offsetWidth.mockRestore();
      clientWidth.mockRestore();
      window.dispatchEvent(new Event("resize"));
      clock.restore();
      vi.unstubAllGlobals();
      Object.defineProperty(window, "innerWidth", innerWidth);
      delete (document.documentElement as { clientWidth?: number }).clientWidth;
    }
  });

  it("recreates WebGL after scaling the terminal font down", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-scale-renderer",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-scale-renderer"
      />,
    );
    await waitFor(() => expect(mockLoadAddon).toHaveBeenCalled());

    const term = vi.mocked(XtermTerminal).mock.results.at(-1)?.value as {
      element?: HTMLElement;
      options: { fontSize: number };
    };
    const zoom = await zoomItem();
    await act(async () => {
      await new Promise(requestAnimationFrame);
    });
    const grid = screen.getByTestId("live-terminal-grid");
    const outer = grid.parentElement!;
    Object.defineProperty(term, "element", {
      configurable: true,
      value: grid,
    });
    Object.defineProperty(grid, "offsetWidth", {
      configurable: true,
      value: 800,
    });
    Object.defineProperty(grid, "offsetHeight", {
      configurable: true,
      value: 480,
    });
    Object.defineProperty(outer, "clientWidth", {
      configurable: true,
      value: 400,
    });

    mockWebglDispose.mockClear();
    mockLoadAddon.mockClear();
    mockRefresh.mockClear();

    await userEvent.setup().click(zoom);

    await waitFor(() => expect(term.options.fontSize).toBe(6));
    await waitFor(() => expect(mockWebglDispose).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(mockLoadAddon).toHaveBeenCalledTimes(1));
    expect(mockRefresh).toHaveBeenCalled();
  });

  it("offers source terminal resize to capture-capable viewers only", async () => {
    const { unmount } = render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-resize",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-resize"
        viewerIsAuthor
      />,
    );
    await openDisplayMenu();
    expect(
      screen.getByRole("menuitem", {
        name: /fit columns and rows to screen/i,
      }),
    ).toBeInTheDocument();

    unmount();
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-resize-ro",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-resize-ro"
        viewerIsAuthor={false}
      />,
    );
    await openDisplayMenu();
    expect(
      screen.queryByRole("menuitem", {
        name: /fit columns and rows to screen/i,
      }),
    ).not.toBeInTheDocument();
  });

  it("keeps viewer display controls available in the Display menu for a read-only desktop viewer", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "read-only-desktop",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="read-only-desktop"
      />,
    );
    expect(await zoomLabel()).toBe("Zoom to fit");
    expect(
      screen.getByRole("menuitem", { name: "Fullscreen" }),
    ).toBeInTheDocument();
  });

  it("names the zoom item for what selecting it does and switches the mode", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-zoom-mode",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-zoom-mode"
      />,
    );
    expect(await zoomLabel()).toBe("Zoom to fit");
    await toggleZoom();
    expect(await zoomLabel()).toBe("Reset zoom");
    await toggleZoom();
    expect(await zoomLabel()).toBe("Zoom to fit");
  });

  it("orders the desktop compact row: New session, Actions, Display", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-order",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-order"
        viewerIsAuthor
      />,
    );
    const names = ["New session", "Actions", "Display"];
    const els = names.map((name) =>
      screen.getByRole("button", { name: new RegExp(`^${name}`) }),
    );
    els.slice(1).forEach((el, i) => {
      expect(
        els[i].compareDocumentPosition(el) & Node.DOCUMENT_POSITION_FOLLOWING,
      ).toBeTruthy();
    });
  });

  it("puts the session tabs to the LEFT of the compact row's control cluster", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        id="sess-left"
        viewerIsAuthor
        privateSessions={[
          {
            id: "sess-left",
            agent: "claude",
            cwd: "/work",
            title: "Only tab",
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
      />,
    );
    const compact = screen.getByTestId("session-compact-row");
    const tab = within(compact).getByRole("tab", { name: /only tab/i });
    const newSession = within(compact).getByRole("button", {
      name: /^new /i,
    });
    expect(
      tab.compareDocumentPosition(newSession) &
        Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
  });

  it("uses a display icon and decorative chevrons for the text menus", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-chevrons",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-chevrons"
        viewerIsAuthor
      />,
    );
    const triggers = [
      screen.getByRole("button", { name: "Actions" }),
      screen.getByRole("button", { name: /^start with/i }),
    ];
    const paths = triggers.map((trigger) => {
      const chevron = trigger.querySelector("svg");
      expect(chevron).toHaveAttribute("aria-hidden", "true");
      return chevron?.querySelector("path")?.getAttribute("d");
    });
    expect(new Set(paths).size).toBe(1);
    expect(triggers[1]).not.toHaveTextContent(/^v$/);
    const display = screen.getByRole("button", { name: "Display" });
    expect(display.textContent).toBe("");
    expect(display.querySelector("svg")).toHaveAttribute("aria-hidden", "true");
  });

  it("opens quick session search from the toolbar search button", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-search-btn",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-search-btn"
        viewerIsAuthor
      />,
    );

    await userEvent
      .setup()
      .click(screen.getByRole("button", { name: /search sessions/i }));

    expect(
      screen.getByRole("dialog", { name: /search sessions/i }),
    ).toBeInTheDocument();
  });

  it("keeps one merged menu with a Display section on a narrow viewport", async () => {
    const original = window.matchMedia;
    window.matchMedia = vi.fn().mockReturnValue({
      matches: true,
      media: "(max-width: 767px)",
      onchange: null,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      addListener: vi.fn(),
      removeListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    try {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-narrow-display",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-narrow-display"
          viewerIsAuthor
        />,
      );
      expect(
        screen.queryByRole("button", { name: "Display" }),
      ).not.toBeInTheDocument();
      await userEvent
        .setup()
        .click(screen.getByRole("button", { name: "Actions" }));
      for (const [role, name] of [
        ["menuitem", "Fit columns and rows to screen"],
        ["menuitem", "Zoom to fit"],
        ["menuitem", "Fullscreen"],
        ["menuitem", "Diagrams"],
        ["menuitem", "Restart session"],
      ] as const) {
        expect(await screen.findByRole(role, { name })).toBeInTheDocument();
      }
    } finally {
      window.matchMedia = original;
    }
  });

  it("collapses secondary actions on a narrow non-touch viewport", async () => {
    const original = window.matchMedia;
    window.matchMedia = vi.fn().mockReturnValue({
      matches: true,
      media: "(max-width: 767px)",
      onchange: null,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      addListener: vi.fn(),
      removeListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });

    try {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-narrow",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-narrow"
          viewerIsAuthor
        />,
      );
      const user = userEvent.setup();
      await user.click(screen.getByRole("button", { name: /^actions$/i }));
      expect(
        await screen.findByRole("menuitem", { name: /sessions overview/i }),
      ).toBeInTheDocument();
    } finally {
      window.matchMedia = original;
    }
  });

  it("drops the standalone Sessions overview link — global nav + Exit fullscreen replace it", () => {
    // The compact session layout removed the duplicate link. Users navigate
    // via the AppShell Sessions link (windowed) or Display → Exit fullscreen
    // (overlay), so no dedicated link lives in the terminal toolbar.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
      />,
      {
        route: "/sessions/sess-1?fullscreen=1",
      },
    );
    expect(
      screen.queryByRole("link", { name: /sessions overview/i }),
    ).not.toBeInTheDocument();
  });

  it("surfaces the notifications bell only in fullscreen, where the header is hidden", async () => {
    const auth = vi.spyOn(api, "authConfig").mockResolvedValue({
      google_enabled: false,
      password_enabled: true,
      user: { email: "reader@example.com", display_name: "Reader" },
    });
    const feed = vi.spyOn(api, "notifications").mockResolvedValue([
      {
        id: "share",
        session_id: "other",
        kind: "share",
        sender_email: "owner@example.com",
        recipient_email: "reader@example.com",
        message: "",
        session_title: "Other session",
        created_at: "2026-10-23T10:00:00Z",
        read_at: null,
        resolved_at: null,
      },
    ]);
    try {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-bell",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-bell"
          viewerIsAuthor
        />,
      );
      // Windowed: the AppShell header owns the bell, so the toolbar has none.
      expect(
        screen.queryByRole("button", { name: /^notifications/i }),
      ).not.toBeInTheDocument();
      await toggleFullscreen();
      const bell = await screen.findByRole("button", {
        name: "Notifications (1 unread)",
      });
      // The bell lives in the compact row's right-aligned control cluster
      // (same cluster as Actions/Display/Settings), ahead of New session.
      expect(
        bell.compareDocumentPosition(
          screen.getByRole("button", { name: /^new /i }),
        ) & Node.DOCUMENT_POSITION_FOLLOWING,
      ).toBeTruthy();
    } finally {
      auth.mockRestore();
      feed.mockRestore();
    }
  });

  it("keeps the live-session switcher in the compact row across windowed and fullscreen", async () => {
    const sessions = [
      {
        id: "sess-1",
        agent: "claude",
        cwd: "/work",
        title: "Alpha",
        messages: 1,
        last_activity: null,
        running: true,
      },
      {
        id: "sess-2",
        agent: "claude",
        cwd: "/work",
        title: "Beta",
        messages: 1,
        last_activity: null,
        running: true,
      },
    ];
    render(
      <LiveTerminal
        privateHostId="host"
        id="sess-1"
        privateSessions={sessions}
      />,
    );
    // Windowed: the switcher lives inside the compact row (the compact row
    // IS the session header now).
    const compact = screen.getByTestId("session-compact-row");
    expect(
      within(compact).getByRole("tablist", { name: /live sessions/i }),
    ).toBeInTheDocument();
    expect(
      within(compact).getByRole("tab", { name: /beta/i }),
    ).toBeInTheDocument();
    await toggleFullscreen();
    // Fullscreen: still inside the compact row (the overlay is just a
    // positioning switch, not a layout fork).
    expect(
      within(screen.getByTestId("session-compact-row")).getByRole("tablist", {
        name: /live sessions/i,
      }),
    ).toBeInTheDocument();
  });

  it("starts the PR and Linear rails collapsed on phones and remembers an explicit expansion", async () => {
    const original = window.matchMedia;
    window.matchMedia = vi.fn().mockReturnValue({
      matches: true,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    localStorage.removeItem("conversations.prRailCollapsed");
    localStorage.removeItem("conversations.linearRailCollapsed");
    mockPrivatePrCacheRead.mockResolvedValue({
      prs: [
        {
          url: "https://github.com/acme/widgets/pull/757",
          owner: "acme",
          repo: "widgets",
          number: 757,
          status: null,
        },
      ],
      comments: [],
    });
    mockPrivateLinearDocs.mockResolvedValue([
      {
        url: "https://linear.app/example/document/spec-phone",
        workspace: "example",
        kind: "document",
        identifier: "spec-phone",
      },
    ]);
    try {
      const mounted = render(
        <LiveTerminal privateHostId="host" id="phone-rails" />,
        {
          route: "/sessions/phone-rails?fullscreen=1",
        },
      );
      const prs = await screen.findByRole("button", {
        name: "Expand pull requests",
      });
      expect(prs).toHaveAttribute("aria-expanded", "false");
      expect(
        await screen.findByRole("button", { name: "Expand Linear" }),
      ).toHaveAttribute("aria-expanded", "false");
      await userEvent.setup().click(prs);
      expect(
        await screen.findByRole("link", { name: /widgets#757/ }),
      ).toBeVisible();
      mounted.unmount();
      render(<LiveTerminal privateHostId="host" id="phone-rails" />, {
        route: "/sessions/phone-rails?fullscreen=1",
      });
      expect(
        await screen.findByTitle("Collapse pull requests"),
      ).toHaveAttribute("aria-expanded", "true");
    } finally {
      window.matchMedia = original;
      localStorage.removeItem("conversations.prRailCollapsed");
      localStorage.removeItem("conversations.linearRailCollapsed");
    }
  });

  it("shows linked Linear refs beside both the windowed and fullscreen terminal", async () => {
    mockPrivateLinearDocs.mockResolvedValue([
      {
        url: "https://linear.app/example-org/document/spec-abc123",
        workspace: "example-org",
        kind: "document",
        identifier: "spec-abc123",
      },
    ]);
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-ld",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-ld"
      />,
    );
    expect(await screen.findByTestId("rail-linear-row")).toBeInTheDocument();
    expect(screen.getByTestId("live-terminal-rail")).toHaveClass("xl:flex");
    // Fullscreen: the Linear rail surfaces the linked ref by its identifier.
    await toggleFullscreen();
    expect(await screen.findByText("spec-abc123")).toBeInTheDocument();
    expect(screen.getByTestId("rail-linear-row")).toBeInTheDocument();
  });

  it("shows detected PR links beside the windowed terminal", async () => {
    mockPrivatePrCacheRead.mockResolvedValue({
      prs: [
        {
          url: "https://github.com/acme/widgets/pull/757",
          owner: "acme",
          repo: "widgets",
          number: 757,
          status: null,
          detected: true,
        },
      ],
      comments: [],
    });
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "windowed-pr",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="windowed-pr"
      />,
    );
    const link = await screen.findByRole("link", { name: /widgets#757/i });
    expect(screen.getByTestId("live-terminal-rail")).toContainElement(link);
    expect(link).toHaveAttribute(
      "href",
      "https://github.com/acme/widgets/pull/757",
    );
    await toggleFullscreen();
    expect(screen.getByTestId("live-terminal-rail")).toHaveClass("flex");
    expect(
      screen.getByRole("link", { name: /widgets#757/i }),
    ).toBeInTheDocument();
  });

  it.each(["prs", "linear"] as const)(
    "stacks newly linked %s above older URLs without changing the cache",
    async (kind) => {
      const id = `sess-stack-${kind}`;
      const links = [1, 2, 3].map((number) =>
        kind === "prs"
          ? {
              url: `https://github.com/acme/widgets/pull/${number}`,
              owner: "acme",
              repo: "widgets",
              number,
              status: null,
            }
          : {
              url: `https://linear.app/example/document/spec-${number}`,
              workspace: "example",
              kind: "document",
              identifier: `spec-${number}`,
            },
      );
      const initial = Object.freeze(links.slice(0, 2));
      const seed = (value: readonly unknown[]) =>
        kind === "prs"
          ? mockPrivatePrCacheRead.mockResolvedValue({
              prs: value,
              comments: [],
            })
          : mockPrivateLinearDocs.mockResolvedValue(value);
      seed(initial);
      let client: QueryClient;
      function CaptureClient() {
        client = useQueryClient();
        return null;
      }
      render(
        <>
          <CaptureClient />
          <LiveTerminal privateHostId="host" id={id} />
        </>,
        { route: `/sessions/${id}?fullscreen=1` },
      );
      const railUrls = () =>
        screen
          .getAllByRole("link")
          .map((link) => link.getAttribute("href"))
          .filter((url) => links.some((link) => link.url === url));
      await waitFor(() =>
        expect(railUrls()).toEqual([links[1].url, links[0].url]),
      );

      const baseKey =
        kind === "prs" ? sessionPrsKey(id) : sessionLinearDocsKey(id);
      // The private source namespaces its cache per host (see sessionPrSource).
      const key = ["private", "host", ...baseKey];
      // The live linked-URL nudge refetches this same shared query cache.
      seed(Object.freeze([...links]));
      act(() => client.setQueryData(key, Object.freeze([...links])));
      await waitFor(() =>
        expect(railUrls()).toEqual([links[2].url, links[1].url, links[0].url]),
      );
      expect(client!.getQueryData(key)).toEqual(links);

      // Closing and reopening fullscreen preserves the stack order.
      await toggleFullscreen();
      await toggleFullscreen();
      await waitFor(() => expect(railUrls()[0]).toBe(links[2].url));
    },
  );

  it("renders no Linear rail in fullscreen when nothing is linked", async () => {
    mockPrivateLinearDocs.mockResolvedValue([]);
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-ld-empty",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-ld-empty"
      />,
      {
        route: "/sessions/sess-ld-empty?fullscreen=1",
      },
    );
    // The query resolves to empty ⇒ the rail returns null (claims no width).
    await waitFor(() =>
      expect(mockPrivateLinearDocs).toHaveBeenCalledWith("sess-ld-empty"),
    );
    expect(screen.queryByTestId("rail-linear-row")).not.toBeInTheDocument();
  });

  it("in the PR rail, hides resolved CodeRabbit rollups and dots open findings by severity", async () => {
    const railPrs = [
      {
        url: "https://github.com/acme/widgets/pull/328",
        owner: "acme",
        repo: "widgets",
        number: 328,
        status: {
          state: "OPEN",
          merged: false,
          draft: false,
          title: "feat: a thing",
        },
        detected: false,
      },
    ];
    const base = {
      owner: "acme",
      repo: "widgets",
      number: 328,
      html_url: "https://github.com/acme/widgets/pull/328",
      ingested_at: "2026-07-01T00:00:10Z",
    };
    mockPrivatePrCacheRead.mockResolvedValue({
      prs: railPrs,
      comments: [
        // A human's review summary — kept.
        {
          ...base,
          comment_id: "review:1",
          kind: "review",
          author: "alice",
          body: "Approved. Looks good to me.",
          created_at: "2026-07-01T00:00:00Z",
        },
        // CodeRabbit's review summary (a rollup) — hidden.
        {
          ...base,
          comment_id: "review:2",
          kind: "review",
          author: "coderabbitai",
          body: "**Actionable comments posted: 4**\n\nwalkthrough…",
          created_at: "2026-07-01T00:01:00Z",
        },
        // A CodeRabbit inline finding that's been resolved — hidden.
        {
          ...base,
          comment_id: "review_comment:10",
          kind: "review_comment",
          author: "coderabbitai",
          resolved: true,
          body: "_📐 Maintainability_ | _🟡 Minor_ | _⚡ Quick win_\n\n**Stale index.**",
          created_at: "2026-07-01T00:02:00Z",
        },
        // A CodeRabbit inline finding still open — shown, with a severity dot.
        {
          ...base,
          comment_id: "review_comment:11",
          kind: "review_comment",
          author: "coderabbitai",
          resolved: false,
          body: "_📐 Maintainability_ | _🟠 Major_ | _⚡ Quick win_\n\n**Spawned task is never aborted.**",
          created_at: "2026-07-01T00:03:00Z",
        },
      ],
    });

    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-pr",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-pr"
      />,
      {
        route: "/sessions/sess-pr?fullscreen=1",
      },
    );

    // The open CodeRabbit finding surfaces by its bold summary (not its raw
    // metadata badge), prefixed with a severity dot labelled by its level.
    expect(
      await screen.findByText("Spawned task is never aborted."),
    ).toBeInTheDocument();
    const dot = screen.getByTestId("rail-coderabbit-severity");
    expect(dot).toHaveAttribute("aria-label", "severity: Major");

    // The human review is kept…
    expect(screen.getByText(/Approved\./)).toBeInTheDocument();
    // …while CodeRabbit's rollup summary and its resolved finding are gone.
    expect(
      screen.queryByText(/Actionable comments posted/),
    ).not.toBeInTheDocument();
    expect(screen.queryByText(/Stale index\./)).not.toBeInTheDocument();
  });

  it("auto-arms capture when an author lands already in fullscreen (tab switch)", () => {
    // Switching live tabs lands a fresh terminal on a ?fullscreen=1 URL — the
    // author is here to drive, so capture arms without a click.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
        viewerIsAuthor
      />,
      {
        route: "/sessions/sess-1?fullscreen=1",
      },
    );
    expect(isCapturing()).toBe(true);
  });

  it("auto-arms capture when author metadata arrives after a fullscreen mount", async () => {
    // A newly-started session can navigate before the live-session poll has
    // delivered `viewer_is_author`. When that metadata catches up, capture must
    // still arm and focus the xterm textarea so typing lands in the new session.
    let setAuthor: ((next: boolean) => void) | undefined;
    function Harness() {
      const [author, setAuthorState] = React.useState(false);
      setAuthor = setAuthorState;
      return (
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-new",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-new"
          viewerIsAuthor={author}
          viewerCanWrite={author}
        />
      );
    }

    render(<Harness />, { route: "/sessions/sess-new?fullscreen=1" });
    expect(
      screen.queryByRole("button", { name: /captur(e|ing) keyboard/i }),
    ).not.toBeInTheDocument();

    act(() => setAuthor?.(true));

    await waitFor(() => expect(isCapturing()).toBe(true));
    expect(mockFocus).toHaveBeenCalled();
  });

  it("waits for CLI attachment before auto-arming fullscreen capture", async () => {
    // A new live row can appear before the host's input/reply streams have
    // subscribed. Do not show "Capturing keyboard" until a keystroke can be
    // delivered.
    let setAttached: ((next: boolean) => void) | undefined;
    function Harness() {
      const [attached, setAttachedState] = React.useState(false);
      setAttached = setAttachedState;
      return (
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-new",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-new"
          viewerIsAuthor
          viewerCanWrite
          cliAttached={attached}
        />
      );
    }

    render(<Harness />, { route: "/sessions/sess-new?fullscreen=1" });
    expect(
      screen.queryByRole("button", { name: /captur(e|ing) keyboard/i }),
    ).not.toBeInTheDocument();

    act(() => setAttached?.(true));

    await waitFor(() => expect(isCapturing()).toBe(true));
    expect(mockFocus).toHaveBeenCalled();
  });

  it("does not auto-arm capture for a non-author landing in fullscreen", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
        viewerIsAuthor={false}
        viewerCanWrite
      />,
      {
        route: "/sessions/sess-1?fullscreen=1",
      },
    );
    expect(isCapturing()).toBe(false);
  });

  it("auto-arms keyboard capture when the host enters fullscreen", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
        viewerIsAuthor
      />,
    );
    // Not capturing initially.
    expect(isCapturing()).toBe(false);
    await toggleFullscreen();
    // Entering fullscreen turned capture on.
    expect(isCapturing()).toBe(true);
  });

  it("forwards ESC to the session while capturing in fullscreen; stops via the toggle, then ESC exits fullscreen", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
        viewerIsAuthor
      />,
    );
    // Host enters fullscreen → capture auto-arms.
    await toggleFullscreen();
    expect(isCapturing()).toBe(true);
    expect(inFullscreen()).toBe(true);

    const pressEscOnDocument = () =>
      act(() =>
        document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" })),
      );

    // While capturing, a document-level ESC belongs to the session, not the
    // fullscreen step-out. If focus is not on xterm's textarea, the capture
    // fallback forwards it directly.
    pressEscOnDocument();
    expect(mockSendInput).toHaveBeenCalledWith("Gw==");
    expect(mockSendInput).toHaveBeenCalledTimes(1);
    expect(isCapturing()).toBe(true);
    expect(inFullscreen()).toBe(true);

    // Every ESC keystroke is forwarded to the session (\x1b → "Gw=="), even
    // repeated ones — there's no double-press escape hatch anymore.
    act(() => onDataCb!("\x1b"));
    act(() => onDataCb!("\x1b"));
    expect(mockSendInput).toHaveBeenCalledWith("Gw==");
    expect(mockSendInput).toHaveBeenCalledTimes(3);
    expect(isCapturing()).toBe(true); // still capturing

    // Stop capture from the toolbar toggle (the only way out now).
    act(() => releaseControl()!.click());
    expect(isCapturing()).toBe(false);
    // Still fullscreen — stopping capture doesn't exit.
    expect(inFullscreen()).toBe(true);

    // Now that capture is off, a document-level ESC leaves fullscreen.
    pressEscOnDocument();
    expect(inFullscreen()).toBe(false);
  });

  it("ESC exits fullscreen in one step when capture is not armed", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
        viewerIsAuthor={false}
        viewerCanWrite
      />,
    );
    await toggleFullscreen();
    // Non-host fullscreen does not arm capture.
    expect(isCapturing()).toBe(false);
    act(() =>
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" })),
    );
    expect(inFullscreen()).toBe(false);
  });

  it("does NOT auto-arm capture for a non-host entering fullscreen", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
        viewerIsAuthor={false}
        viewerCanWrite
      />,
    );
    await toggleFullscreen();
    expect(isCapturing()).toBe(false);
  });

  it("shows no keyboard control on desktop while idle", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-idle",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-idle"
        viewerIsAuthor
      />,
    );
    expect(
      screen.queryByRole("button", { name: /keyboard/i }),
    ).not.toBeInTheDocument();
    expect(screen.getByText("Click the terminal to type.")).toBeInTheDocument();
  });

  it("shows a status while capturing that releases capture and stops forwarding keys", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-release",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-release"
        viewerIsAuthor
      />,
    );
    armCapture();
    expect(screen.getByText("Keyboard captured")).toBeInTheDocument();
    expect(
      screen.queryByText("Click the terminal to type."),
    ).not.toBeInTheDocument();
    act(() => onDataCb!("a"));
    expect(mockSendInput).toHaveBeenCalledTimes(1);

    await userEvent
      .setup()
      .click(screen.getByRole("button", { name: /release keyboard/i }));

    expect(isCapturing()).toBe(false);
    expect(screen.queryByText("Keyboard captured")).not.toBeInTheDocument();
    act(() => onDataCb!("b"));
    expect(mockSendInput).toHaveBeenCalledTimes(1);
  });

  it.each([
    ["a non-author without write access", { viewerIsAuthor: false }],
    [
      "a viewer whose CLI is not attached",
      { viewerIsAuthor: true, cliAttached: false },
    ],
    [
      "a viewer without author or write grant",
      { viewerIsAuthor: false, viewerCanWrite: false },
    ],
  ])("does not arm capture on a terminal click for %s", (_name, props) => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
        {...props}
      />,
    );
    armCapture();
    expect(isCapturing()).toBe(false);
    expect(
      screen.queryByText("Click the terminal to type."),
    ).not.toBeInTheDocument();
  });

  it("arms capture for a non-author granted write access", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-1",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-1"
        viewerIsAuthor={false}
        viewerCanWrite
      />,
    );
    armCapture();
    expect(isCapturing()).toBe(true);
  });

  it("keeps the release status reachable in the fullscreen toolbar", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-fs-release",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-fs-release"
        viewerIsAuthor
      />,
      {
        route: "/sessions/sess-fs-release?fullscreen=1",
      },
    );
    expect(inFullscreen()).toBe(true);
    expect(isCapturing()).toBe(true);
    await userEvent
      .setup()
      .click(screen.getByRole("button", { name: /release keyboard/i }));
    expect(isCapturing()).toBe(false);
    expect(inFullscreen()).toBe(true);
  });

  it("forwards document keydowns while capturing when xterm has lost focus", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-doc",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-doc"
        viewerIsAuthor
      />,
    );
    armCapture();

    act(() =>
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "a" })),
    );
    act(() =>
      document.dispatchEvent(
        new KeyboardEvent("keydown", { key: "c", ctrlKey: true }),
      ),
    );
    act(() =>
      document.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Backspace" }),
      ),
    );

    expect(mockSendInput).toHaveBeenNthCalledWith(1, "YQ==");
    expect(mockSendInput).toHaveBeenNthCalledWith(2, "Aw==");
    expect(mockSendInput).toHaveBeenNthCalledWith(3, "fw==");
  });

  it("forwards an escape sequence (arrow key) and a bare ESC", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-x",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-x"
        viewerIsAuthor
      />,
    );
    armCapture();
    expect(onDataCb).not.toBeNull();

    // Arrow-up: a multi-char escape sequence — MUST be forwarded.
    act(() => onDataCb!("\x1b[A"));
    // btoa of UTF-8 bytes of "\x1b[A" (0x1b 0x5b 0x41) = "G1tB".
    expect(mockSendInput).toHaveBeenCalledWith("G1tB");

    mockSendInput.mockClear();
    // A bare ESC is FORWARDED (claude interrupt / clear) and capture stays on.
    act(() => onDataCb!("\x1b"));
    expect(mockSendInput).toHaveBeenCalledWith("Gw==");
    expect(isCapturing()).toBe(true);
    // Keystrokes are signed host operations; the central terminal socket is
    // never an input path.
    expect(mockSocketSendInput).not.toHaveBeenCalled();
  });

  it("does not also send a link click to the session as a mouse report", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-x",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-x"
        viewerIsAuthor
      />,
    );
    armCapture();

    const { linkHandler } = vi.mocked(XtermTerminal).mock.calls.at(-1)![0]!;
    const link = new MouseEvent("mousemove");
    const range = {} as never;
    linkHandler!.hover!(link, "https://example.com", range);
    act(() => {
      onDataCb!("\x1b[<0;3;2M");
      onDataCb!("\x1b[<0;3;2m");
    });
    expect(mockSendInput).not.toHaveBeenCalled();

    // Wheel over a link still scrolls the TUI.
    act(() => onDataCb!("\x1b[<64;3;2M"));
    expect(mockSendInput).toHaveBeenCalledTimes(1);

    linkHandler!.leave!(link, "https://example.com", range);
    mockSendInput.mockClear();
    act(() => onDataCb!("\x1b[<0;3;2M"));
    expect(mockSendInput).toHaveBeenCalledTimes(1);
  });

  it("still sends ordinary clicks to the session as mouse reports", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-x",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-x"
        viewerIsAuthor
      />,
    );
    armCapture();

    act(() => {
      onDataCb!("\x1b[<0;3;2M");
      onDataCb!("\x1b[<0;3;2m");
    });
    expect(mockSendInput).toHaveBeenNthCalledWith(1, btoa("\x1b[<0;3;2M"));
    expect(mockSendInput).toHaveBeenNthCalledWith(2, btoa("\x1b[<0;3;2m"));
  });

  it("accelerates OpenCode wheel input in one PTY frame", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-oc",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-oc"
        agent="opencode"
        viewerIsAuthor
      />,
    );
    armCapture();
    expect(customWheelHandler).not.toBeNull();
    expect(onDataCb).not.toBeNull();

    const wheelReport = "\x1b[<65;12;8M";
    act(() => {
      expect(customWheelHandler!(new WheelEvent("wheel", { deltaY: 24 }))).toBe(
        true,
      );
      // xterm emits its encoded mouse report synchronously from the same wheel
      // dispatch, after the custom handler returns.
      onDataCb!(wheelReport);
    });

    expect(mockSendInput).toHaveBeenCalledTimes(1);
    expect(mockSendInput).toHaveBeenCalledWith(btoa(wheelReport.repeat(3)));
  });

  it("does not accelerate another agent's wheel input", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-codex",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-codex"
        agent="codex"
        viewerIsAuthor
      />,
    );
    armCapture();

    const wheelReport = "\x1b[<64;12;8M";
    act(() => {
      customWheelHandler!(new WheelEvent("wheel", { deltaY: -24 }));
      onDataCb!(wheelReport);
    });

    expect(mockSendInput).toHaveBeenCalledTimes(1);
    expect(mockSendInput).toHaveBeenCalledWith(btoa(wheelReport));
  });

  it("does not accelerate later OpenCode keyboard input when a wheel emits nothing", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-oc-small-wheel",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-oc-small-wheel"
        agent="opencode"
        viewerIsAuthor
      />,
    );
    armCapture();

    act(() => {
      customWheelHandler!(new WheelEvent("wheel", { deltaY: 0.25 }));
    });
    // xterm accumulates sub-cell trackpad deltas and emits no onData payload for
    // some wheel events. The wheel marker must expire before the next keypress.
    await act(async () => {
      await Promise.resolve();
    });
    act(() => onDataCb!("a"));

    expect(mockSendInput).toHaveBeenCalledTimes(1);
    expect(mockSendInput).toHaveBeenCalledWith("YQ==");
  });

  it("forwards OpenCode wheel input for a write viewer without keyboard capture", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-oc-wheel",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-oc-wheel"
        agent="opencode"
        viewerCanWrite
      />,
    );
    // Capture is NOT armed — scrolling must still reach the session.
    expect(isCapturing()).toBe(false);
    expect(onDataCb).not.toBeNull();
    const term = vi.mocked(XtermTerminal).mock.results.at(-1)?.value;

    const wheelReport = "\x1b[<65;12;8M";
    act(() => {
      customWheelHandler!(new WheelEvent("wheel", { deltaY: 24 }));
      // stdin is lifted for just this synchronous dispatch, so xterm's mouse
      // report can reach onData even though capture is off.
      expect(term.options.disableStdin).toBe(false);
      onDataCb!(wheelReport);
    });

    expect(mockSendInput).toHaveBeenCalledTimes(1);
    expect(mockSendInput).toHaveBeenCalledWith(btoa(wheelReport.repeat(3)));

    // The lift ends with the dispatch (microtask) — keystrokes stay locked out.
    await act(async () => {
      await Promise.resolve();
    });
    expect(term.options.disableStdin).toBe(true);
  });

  it("does not forward keystrokes for a write viewer while capture is off", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-write-keys",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-write-keys"
        viewerCanWrite
      />,
    );
    // Subscribed for wheel forwarding, but keyboard input must not flow.
    expect(onDataCb).not.toBeNull();

    act(() => onDataCb!("a"));
    expect(mockSendInput).not.toHaveBeenCalled();
  });

  it("forwards no wheel input and keeps stdin locked for a read-only viewer", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-ro-wheel",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-ro-wheel"
        agent="opencode"
      />,
    );
    expect(onDataCb).toBeNull();
    const term = vi.mocked(XtermTerminal).mock.results.at(-1)?.value;

    act(() => {
      customWheelHandler!(new WheelEvent("wheel", { deltaY: 24 }));
    });

    expect(term.options.disableStdin).toBe(true);
    expect(mockSendInput).not.toHaveBeenCalled();
  });

  it("keeps capturing through repeated ESC (no double-ESC escape hatch)", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-x",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-x"
        viewerIsAuthor
      />,
    );
    armCapture();
    expect(isCapturing()).toBe(true);

    // Two ESCs in quick succession: both forwarded, capture stays on — there's
    // no keyboard escape hatch, so the toggle is the only way to stop.
    act(() => onDataCb!("\x1b"));
    act(() => onDataCb!("\x1b"));
    expect(mockSendInput).toHaveBeenCalledWith("Gw==");
    expect(mockSendInput).toHaveBeenCalledTimes(2);
    expect(isCapturing()).toBe(true);
  });

  it("rewrites Shift+Enter to Meta+Enter (newline) instead of submitting", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-se",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-se"
        viewerIsAuthor
      />,
    );
    armCapture();
    expect(customKeyHandler).not.toBeNull();

    const handled = customKeyHandler!(
      new KeyboardEvent("keydown", { key: "Enter", shiftKey: true }),
    );
    // Returning false tells xterm to swallow its default CR.
    expect(handled).toBe(false);
    // Meta+Enter "\x1b\r" (0x1b 0x0d) → base64 "Gw0=".
    expect(mockSendInput).toHaveBeenCalledWith("Gw0=");
  });

  it("lets F5 stay a browser refresh key while capture is armed", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-f5",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-f5"
        viewerIsAuthor
      />,
    );
    armCapture();
    expect(customKeyHandler).not.toBeNull();

    const event = new KeyboardEvent("keydown", {
      key: "F5",
      cancelable: true,
    });
    const handled = customKeyHandler!(event);

    expect(handled).toBe(false);
    expect(event.defaultPrevented).toBe(false);
    expect(mockSendInput).not.toHaveBeenCalled();
  });

  it("does not intercept Shift+Enter when capture is off", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-se",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-se"
        viewerIsAuthor
      />,
    );
    // Capture not armed.
    expect(customKeyHandler).not.toBeNull();
    const handled = customKeyHandler!(
      new KeyboardEvent("keydown", { key: "Enter", shiftKey: true }),
    );
    expect(handled).toBe(true); // let xterm handle it normally
    expect(mockSendInput).not.toHaveBeenCalled();
  });

  it.each([false, true])(
    "keeps Ctrl+V in the browser with shift=%s",
    (shiftKey) => {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-paste",
              agent: "codex",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-paste"
          viewerIsAuthor
        />,
      );
      armCapture();
      const event = new KeyboardEvent("keydown", {
        key: "v",
        ctrlKey: true,
        shiftKey,
        bubbles: true,
        cancelable: true,
      });
      expect(customKeyHandler!(event)).toBe(false);
      expect(event.defaultPrevented).toBe(false);

      // Losing textarea focus must not turn browser paste into remote Ctrl+V.
      (document.activeElement as HTMLElement)?.blur();
      document.body.dispatchEvent(event);
      expect(event.defaultPrevented).toBe(false);
      expect(mockSendInput).not.toHaveBeenCalled();
    },
  );

  it("uses the declared clipboard MIME when the image file has no type", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-clip",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-clip"
        viewerIsAuthor
      />,
    );
    armCapture();

    // PNG magic bytes 0x89 'P' 'N' 'G' → base64 "iVBORw==".
    const bytes = new Uint8Array([0x89, 0x50, 0x4e, 0x47]);
    const file = {
      type: "",
      size: bytes.length,
      arrayBuffer: () => Promise.resolve(bytes.buffer),
    } as unknown as File;
    const ev = new Event("paste", { bubbles: true });
    Object.defineProperty(ev, "clipboardData", {
      value: {
        items: [{ type: "image/png", getAsFile: () => file }],
      },
    });
    act(() => {
      document.dispatchEvent(ev);
    });

    await waitFor(() =>
      expect(mockPasteImage).toHaveBeenCalledWith(
        "sess-clip",
        "image/png",
        bytes,
        {},
        "host",
      ),
    );
  });

  it("tries no other path when the host refuses a pasted image", async () => {
    mockPasteImage.mockRejectedValueOnce(new Error("refused"));
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-clip-refused",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-clip-refused"
        viewerIsAuthor
      />,
    );
    armCapture();
    const bytes = new Uint8Array([0x89, 0x50, 0x4e, 0x47]);
    const file = {
      type: "image/png",
      size: bytes.length,
      arrayBuffer: () => Promise.resolve(bytes.buffer),
    } as unknown as File;
    const ev = new Event("paste", { bubbles: true });
    Object.defineProperty(ev, "clipboardData", {
      value: { items: [{ type: "image/png", getAsFile: () => file }] },
    });
    act(() => {
      document.dispatchEvent(ev);
    });
    await waitFor(() => expect(mockPasteImage).toHaveBeenCalled());
    await new Promise((r) => setTimeout(r, 20));
    expect(mockPasteImage).toHaveBeenCalledTimes(1);
    expect(mockSendFiles).not.toHaveBeenCalled();
  });

  it("does not forward a paste when capture is off", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-clip-off",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-clip-off"
        viewerIsAuthor
      />,
    );
    const bytes = new Uint8Array([0x89, 0x50, 0x4e, 0x47]);
    const file = {
      type: "image/png",
      size: bytes.length,
      arrayBuffer: () => Promise.resolve(bytes.buffer),
    } as unknown as File;
    const ev = new Event("paste", { bubbles: true });
    Object.defineProperty(ev, "clipboardData", {
      value: { items: [{ type: "image/png", getAsFile: () => file }] },
    });
    act(() => {
      document.dispatchEvent(ev);
    });
    expect(mockPasteImage).not.toHaveBeenCalled();
  });

  // ── Slack-friendly copy ──────────────────────────────────────────────────
  // Build a one-line buffer where `codeText` carries palette index 6 (the rest
  // is default fg) and select the whole line.
  function armCodeSelection(plain: string, codeText: string) {
    const full = plain + codeText;
    const mkCell = (ch: string, isCode: boolean) => ({
      getChars: () => ch,
      getWidth: () => 1,
      getFgColor: () => (isCode ? 6 : -1),
      isFgDefault: () => !isCode,
      isFgPalette: () => isCode,
      isFgRGB: () => false,
    });
    const cells = [...full].map((ch, i) => mkCell(ch, i >= plain.length));
    lineState = [
      {
        length: cells.length,
        isWrapped: false,
        getCell: (x: number) => cells[x],
      },
    ];
    selectionState = { start: { x: 0, y: 0 }, end: { x: full.length, y: 0 } };
  }

  function dispatchCopy(el: Element) {
    const setData = vi.fn();
    const ev = new Event("copy", { bubbles: true, cancelable: true });
    Object.defineProperty(ev, "clipboardData", { value: { setData } });
    act(() => {
      el.dispatchEvent(ev);
    });
    return { setData, prevented: ev.defaultPrevented };
  }

  it("enriches a terminal copy with Slack-friendly inline code", () => {
    localStorage.setItem(
      "arrakisTermCodeColors",
      JSON.stringify({ palette: [6] }),
    );
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-copy",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-copy"
      />,
    );
    armCodeSelection("set ", "active=f");

    const { setData, prevented } = dispatchCopy(
      screen.getByTestId("live-terminal-grid"),
    );

    expect(setData).toHaveBeenCalledWith("text/plain", "set `active=f`");
    expect(setData).toHaveBeenCalledWith(
      "text/html",
      "set <code>active=f</code>",
    );
    expect(prevented).toBe(true);
  });

  it("leaves xterm's default copy alone when nothing is selected", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-copy-empty",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-copy-empty"
      />,
    );
    // selectionState stays undefined (no selection).
    const { setData, prevented } = dispatchCopy(
      screen.getByTestId("live-terminal-grid"),
    );
    expect(setData).not.toHaveBeenCalled();
    expect(prevented).toBe(false);
  });

  it("base64-encodes multi-byte UTF-8 keystrokes correctly", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-u",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-u"
        viewerIsAuthor
      />,
    );
    armCapture();
    // "é" → UTF-8 bytes 0xC3 0xA9 → base64 "w6k=".
    act(() => onDataCb!("é"));
    expect(mockSendInput).toHaveBeenCalledWith("w6k=");
  });

  it("focuses the terminal synchronously when arming capture (iOS soft keyboard)", () => {
    // The iOS soft keyboard only appears when the textarea is focused INSIDE
    // the gesture; the capture toggle must focus on click, not from an effect.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-f",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-f"
        viewerIsAuthor
      />,
    );
    expect(mockFocus).not.toHaveBeenCalled();
    armCapture();
    expect(mockFocus).toHaveBeenCalled();
  });

  it("arms keyboard capture (and focuses) when a capture-capable viewer clicks the terminal", () => {
    // A host shouldn't have to find the toolbar toggle — clicking the screen
    // arms capture and focuses xterm so the next keystroke lands.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-click",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-click"
        viewerIsAuthor
      />,
    );
    expect(isCapturing()).toBe(false);

    act(() => fireEvent.click(screen.getByTestId("live-terminal-screen")));

    expect(isCapturing()).toBe(true);
    expect(mockFocus).toHaveBeenCalled();
  });

  it("does NOT arm capture when a read-only viewer clicks the terminal", () => {
    // No control grant ⇒ the click is not hijacked; xterm keeps native
    // click-to-select / scroll and nothing is sent to the session.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-ro",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-ro"
        viewerIsAuthor={false}
        viewerCanWrite={false}
      />,
    );
    act(() => fireEvent.click(screen.getByTestId("live-terminal-screen")));
    // A read-only viewer has no capture toggle at all, so none can arm.
    expect(
      screen.queryByRole("button", { name: /captur(e|ing) keyboard/i }),
    ).not.toBeInTheDocument();
    expect(mockSendInput).not.toHaveBeenCalled();
  });

  it("releases keyboard capture when the viewer clicks outside the terminal (windowed)", () => {
    // Corollary to click-to-capture: a pointer-down anywhere off the widget
    // hands the keyboard back to the page so the next keystroke isn't swallowed.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-away",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-away"
        viewerIsAuthor
      />,
    );
    armCapture();
    expect(isCapturing()).toBe(true);

    act(() => fireEvent.pointerDown(document.body));

    expect(isCapturing()).toBe(false);
  });

  it("keeps capturing when a pointer-down lands inside the terminal widget", () => {
    // "Outside" is the whole widget, not just the screen: clicking the
    // compact-row itself (its container) or the terminal must NOT drop
    // capture mid-use. We assert against the compact-row container rather
    // than a Radix dropdown trigger because the trigger's own pointer-event
    // handling is what's being bypassed when a user drags across it.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-inside",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-inside"
        viewerIsAuthor
      />,
    );
    armCapture();
    expect(isCapturing()).toBe(true);

    act(() => fireEvent.pointerDown(screen.getByTestId("session-compact-row")));
    expect(isCapturing()).toBe(true);

    // The screen itself (the terminal area proper) is inside too.
    act(() =>
      fireEvent.pointerDown(screen.getByTestId("live-terminal-screen")),
    );
    expect(isCapturing()).toBe(true);
  });

  it("does NOT release capture on an outside click while fullscreen", () => {
    // Fullscreen owns the viewport — there's nothing meaningful to click away
    // to, and capture is released via the toggle / ESC, not click-away. The
    // author landing already-fullscreen auto-arms capture.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-fs-away",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-fs-away"
        viewerIsAuthor
      />,
      {
        route: "/sessions/sess-fs-away?fullscreen=1",
      },
    );
    expect(isCapturing()).toBe(true);

    act(() => fireEvent.pointerDown(document.body));

    expect(isCapturing()).toBe(true);
  });
});

// Touch-device behaviour is gated on `navigator.maxTouchPoints`; drive it here.
describe("LiveTerminal (touch device)", () => {
  let original: PropertyDescriptor | undefined;
  async function selectMenuItem(name: RegExp) {
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: /^actions$/i }));
    await user.click(await screen.findByRole("menuitem", { name }));
  }

  beforeEach(() => {
    original = Object.getOwnPropertyDescriptor(navigator, "maxTouchPoints");
    Object.defineProperty(navigator, "maxTouchPoints", {
      configurable: true,
      value: 5,
    });
  });
  afterEach(() => {
    if (original) Object.defineProperty(navigator, "maxTouchPoints", original);
    else delete (navigator as { maxTouchPoints?: number }).maxTouchPoints;
  });

  it("pastes through xterm only on a connected writable captured terminal", async () => {
    const originalClipboard = Object.getOwnPropertyDescriptor(
      navigator,
      "clipboard",
    );
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { readText: vi.fn().mockResolvedValue("hello\n世界") },
    });
    try {
      const { unmount } = render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "mobile-paste",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="mobile-paste"
          viewerIsAuthor
        />,
      );
      expect(
        screen.queryByRole("button", { name: "Paste" }),
      ).not.toBeInTheDocument();
      act(() => capturedHandlers!.onFrame(80, 24, makeBytes([65])));
      fireEvent.click(screen.getByTestId("live-terminal-screen"));
      await act(async () =>
        fireEvent.click(screen.getByRole("button", { name: "Paste" })),
      );
      expect(mockPaste).toHaveBeenCalledExactlyOnceWith("hello\n世界");
      unmount();
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "mobile-paste",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="mobile-paste"
          viewerIsAuthor={false}
        />,
      );
      expect(
        screen.queryByRole("button", { name: "Paste" }),
      ).not.toBeInTheDocument();
    } finally {
      if (originalClipboard)
        Object.defineProperty(navigator, "clipboard", originalClipboard);
      else delete (navigator as { clipboard?: Clipboard }).clipboard;
    }
  });

  it("shows a tap-to-type hint to a capture-capable viewer before capturing", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-t",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-t"
        viewerIsAuthor
      />,
    );
    expect(screen.getByText(/tap the terminal to type/i)).toBeInTheDocument();
  });

  it("shows no tap-to-type hint to a read-only viewer", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-t2",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-t2"
        viewerIsAuthor={false}
      />,
    );
    expect(
      screen.queryByText(/tap the terminal to type/i),
    ).not.toBeInTheDocument();
  });

  it("offers session search explicitly in the touch menu", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-search",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-search"
        viewerIsAuthor
      />,
    );
    await selectMenuItem(/search sessions/i);

    expect(
      screen.getByRole("dialog", { name: /search sessions/i }),
    ).toBeInTheDocument();
  });

  it("renders an on-screen key bar while capturing and sends raw bytes on press", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-k",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-k"
        viewerIsAuthor
      />,
    );
    await selectMenuItem(/capture keyboard/i);
    // Esc button → base64 of "\x1b" (0x1b) = "Gw==".
    const esc = screen.getByRole("button", { name: /escape/i });
    fireEvent.pointerDown(esc);
    expect(mockSendInput).toHaveBeenCalledWith("Gw==");
    // Enter button → base64 of "\r" (0x0d) = "DQ==".
    mockSendInput.mockClear();
    fireEvent.pointerDown(screen.getByRole("button", { name: /enter/i }));
    expect(mockSendInput).toHaveBeenCalledWith("DQ==");
  });

  it("sends the right raw bytes for each on-screen control key", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-keys",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-keys"
        viewerIsAuthor
      />,
    );
    await selectMenuItem(/capture keyboard/i);
    // label → expected base64 of the key's raw escape sequence.
    const cases: ReadonlyArray<[RegExp, string]> = [
      [/^tab/i, "CQ=="], // "\t" (0x09)
      [/up arrow/i, "G1tB"], // "\x1b[A"
      [/down arrow/i, "G1tC"], // "\x1b[B"
      [/left arrow/i, "G1tE"], // "\x1b[D"
      [/right arrow/i, "G1tD"], // "\x1b[C"
      [/ctrl-c/i, "Aw=="], // "\x03"
    ];
    for (const [name, b64] of cases) {
      mockSendInput.mockClear();
      fireEvent.pointerDown(screen.getByRole("button", { name }));
      expect(mockSendInput).toHaveBeenCalledWith(b64);
    }
  });

  it("hides the on-screen key bar when not capturing", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-nk",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-nk"
        viewerIsAuthor
      />,
    );
    expect(
      screen.queryByRole("button", { name: /escape/i }),
    ).not.toBeInTheDocument();
  });

  it("arms capture and focuses the textarea when the author taps the terminal (soft keyboard)", () => {
    // iOS only raises the soft keyboard when the textarea is focused INSIDE the
    // tap gesture, so the terminal tap must both arm capture and focus now.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-tap",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-tap"
        viewerIsAuthor
      />,
    );
    // Key bar absent before capturing.
    expect(
      screen.queryByRole("button", { name: /escape/i }),
    ).not.toBeInTheDocument();

    act(() => fireEvent.click(screen.getByTestId("live-terminal-screen")));

    // Capturing now ⇒ the on-screen key bar appears, and focus was requested
    // synchronously inside the gesture.
    expect(screen.getByRole("button", { name: /escape/i })).toBeInTheDocument();
    expect(mockFocus).toHaveBeenCalled();
  });

  it("enters fullscreen when a terminal tap starts keyboard capture", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-tap-fullscreen",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-tap-fullscreen"
        viewerIsAuthor
      />,
    );

    act(() => fireEvent.click(screen.getByTestId("live-terminal-screen")));

    await waitFor(() =>
      expect(screen.getByTestId("live-terminal-screen")).toHaveClass(
        "overflow-auto",
      ),
    );
  });

  it("enters fullscreen when the mobile menu starts keyboard capture", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-menu-fullscreen",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-menu-fullscreen"
        viewerIsAuthor={false}
        viewerCanWrite
      />,
    );

    await selectMenuItem(/capture keyboard/i);

    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: /^actions$/i }));
    expect(
      await screen.findByRole("menuitem", { name: /exit fullscreen/i }),
    ).toBeInTheDocument();
  });

  it("stops capture when the mobile menu exits fullscreen", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-exit-fullscreen",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-exit-fullscreen"
        viewerIsAuthor
      />,
    );

    await selectMenuItem(/capture keyboard/i);
    await selectMenuItem(/exit fullscreen/i);

    await waitFor(() =>
      expect(screen.getByTestId("live-terminal-screen")).toHaveClass(
        "overflow-hidden",
      ),
    );
    expect(
      screen.queryByRole("button", { name: /escape/i }),
    ).not.toBeInTheDocument();
    expect(screen.getByText(/tap the terminal to type/i)).toBeInTheDocument();
  });

  it("re-focuses the terminal on a tap while already capturing (no toggle off)", () => {
    // A tap should never turn capture OFF — it only (re)focuses so the soft
    // keyboard comes back after the user dismissed it.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-tap2",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-tap2"
        viewerIsAuthor
      />,
    );
    const tap = () =>
      act(() => fireEvent.click(screen.getByTestId("live-terminal-screen")));
    tap(); // arm
    expect(screen.getByRole("button", { name: /escape/i })).toBeInTheDocument();
    mockFocus.mockClear();
    tap(); // tap again while capturing
    expect(screen.getByRole("button", { name: /escape/i })).toBeInTheDocument();
    expect(mockFocus).toHaveBeenCalled();
  });

  it("restores terminal focus when the menu trigger closes during capture", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-menu-focus",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-menu-focus"
        viewerIsAuthor
      />,
    );
    act(() => fireEvent.click(screen.getByTestId("live-terminal-screen")));
    expect(screen.getByRole("button", { name: /escape/i })).toBeInTheDocument();
    mockFocus.mockClear();

    const user = userEvent.setup();
    const trigger = screen.getByRole("button", { name: /^actions$/i });
    await user.click(trigger);
    expect(await screen.findByRole("menu")).toBeInTheDocument();
    fireEvent.pointerDown(trigger, { button: 0, ctrlKey: false });

    await waitFor(() => expect(mockFocus).toHaveBeenCalled());
    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /escape/i })).toBeInTheDocument();
  });

  it("does NOT arm capture when a read-only viewer taps the terminal", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-tap-ro",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-tap-ro"
        viewerIsAuthor={false}
      />,
    );
    act(() => fireEvent.click(screen.getByTestId("live-terminal-screen")));
    // No capture toggle for a read-only viewer, and nothing reaches the session.
    expect(
      screen.queryByRole("button", { name: /captur(e|ing) keyboard/i }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /escape/i }),
    ).not.toBeInTheDocument();
    expect(mockSendInput).not.toHaveBeenCalled();
  });

  it("keeps capture when the viewer taps outside after mobile auto-fullscreen", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-touch-away",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-touch-away"
        viewerIsAuthor
      />,
    );
    await selectMenuItem(/capture keyboard/i);
    // On touch, arming shows the on-screen key bar and enters fullscreen.
    expect(screen.getByRole("button", { name: /escape/i })).toBeInTheDocument();
    await waitFor(() =>
      expect(screen.getByTestId("live-terminal-screen")).toHaveClass(
        "overflow-auto",
      ),
    );

    act(() => fireEvent.pointerDown(document.body));

    // Fullscreen owns the viewport, so click-away does not release capture.
    expect(screen.getByRole("button", { name: /escape/i })).toBeInTheDocument();
  });

  it("keeps capturing when the on-screen key bar is tapped (inside the widget)", async () => {
    // The key bar is part of the widget — a tap on it must NOT count as an
    // outside click (which would drop capture and hide the bar mid-use).
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-bar",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-bar"
        viewerIsAuthor
      />,
    );
    await selectMenuItem(/capture keyboard/i);
    expect(screen.getByRole("button", { name: /escape/i })).toBeInTheDocument();

    fireEvent.pointerDown(screen.getByRole("button", { name: /escape/i }));

    expect(screen.getByRole("button", { name: /escape/i })).toBeInTheDocument();
  });

  it("defaults Zoom to fit ON so the grid fits a short (landscape) viewport", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-fit",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-fit"
        viewerIsAuthor
      />,
    );
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: /^actions$/i }));
    // fit defaults to true on touch, so the item offers to undo it.
    await user.click(
      await screen.findByRole("menuitem", { name: "Reset zoom" }),
    );
    await user.click(screen.getByRole("button", { name: /^actions$/i }));
    expect(
      await screen.findByRole("menuitem", { name: "Zoom to fit" }),
    ).toBeInTheDocument();
  });

  it("puts phone fitting and session actions in the top bar menu", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-more",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-more"
        viewerIsAuthor
      />,
    );

    // Session actions and source grid resizing stay in the top bar menu.
    expect(
      screen.queryByRole("button", { name: /captur(e|ing) keyboard/i }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /new session/i }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /fit phone/i }),
    ).not.toBeInTheDocument();
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: /^actions$/i }));
    expect(
      await screen.findByRole("menuitem", { name: /capture keyboard/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("menuitem", { name: /new claude session/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("menuitem", { name: /new codex session/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("menuitem", { name: /new zsh session/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("menuitem", { name: /sessions overview/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("menuitem", { name: "Reset zoom" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("menuitem", {
        name: /fit columns and rows to screen/i,
      }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("menuitem", { name: /^fullscreen$/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("menuitem", { name: /end session/i }),
    ).toBeInTheDocument();
  });

  it("projects a portrait grid and asks the CLI host to resize it", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-phone-fit",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-phone-fit"
        viewerIsAuthor
      />,
      {
        route: "/sessions/sess-phone-fit?fullscreen=1",
      },
    );

    act(() => capturedHandlers?.onFrame(80, 24, new Uint8Array([72, 105])));
    measureTerminal({ viewportWidth: 390, viewportHeight: 720 });

    await selectMenuItem(/fit columns and rows to screen/i);

    // 80x24 rendered at 640x384 with a 13px font projects to 39x36 at
    // the touch target font (16px) inside a 390x720 portrait viewport.
    await waitFor(() =>
      expect(mockResizeLive).toHaveBeenCalledWith("sess-phone-fit", 39, 36),
    );
  });

  it("fits a newly started browser session once after its initial replay", async () => {
    const clock = installAnimationFrameClock();
    try {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-new-fit",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-new-fit"
          viewerIsAuthor
        />,
        {
          route: {
            pathname: "/sessions/sess-new-fit",
            state: { fitNewSession: "sess-new-fit" },
          },
        },
      );
      measureTerminal({ viewportWidth: 800, viewportHeight: 600 });
      expect(mockResizeLive).not.toHaveBeenCalled();

      act(() => capturedHandlers?.onFrame(80, 24, new Uint8Array([72, 105])));
      await waitFor(() => expect(mockWrite).toHaveBeenCalled());
      act(() => capturedHandlers?.onReplayComplete?.());
      await waitFor(() =>
        expect(screen.queryByText(/loading terminal/i)).not.toBeInTheDocument(),
      );
      expect(clock.pending).toBeGreaterThan(0);
      for (let i = 0; i < 8 && !mockResizeLive.mock.calls.length; i++) {
        act(() => clock.advance());
      }
      await waitFor(() =>
        expect(mockResizeLive).toHaveBeenCalledExactlyOnceWith(
          "sess-new-fit",
          81,
          30,
        ),
      );
      act(() => capturedHandlers?.onFrame(81, 30, new Uint8Array([33])));
      act(() => clock.advance());
      expect(mockResizeLive).toHaveBeenCalledTimes(1);
    } finally {
      clock.restore();
    }
  });

  it("goes fullscreen before fitting, so the grid matches the screen it is read on", async () => {
    // Regression: resizing from the windowed card measured the card,
    // producing a grid that filled a fraction of the fullscreen overlay the
    // phone then views the session in — text in a block at the top, dead space
    // below, and no scroll left to reach anything.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-phone-windowed",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-phone-windowed"
        viewerIsAuthor
      />,
    );

    act(() => capturedHandlers?.onFrame(80, 24, new Uint8Array([72, 105])));
    measureTerminal({ viewportWidth: 390, viewportHeight: 720 });

    // Record the box's layout AT the moment the resize is posted: waiting for
    // fullscreen and for the call separately would also pass if the call went
    // out first, off the windowed measurement.
    let boxClassAtResize = "";
    mockResizeLive.mockImplementationOnce(() => {
      boxClassAtResize = screen.getByTestId("live-terminal-screen").className;
      return Promise.resolve({ delivered: true });
    });

    await selectMenuItem(/fit columns and rows to screen/i);

    // The same 39x36 portrait projection the already-fullscreen path makes…
    await waitFor(() =>
      expect(mockResizeLive).toHaveBeenCalledWith(
        "sess-phone-windowed",
        39,
        36,
      ),
    );
    // …measured with the overlay already up (`overflow-auto` is fullscreen-only).
    expect(boxClassAtResize).toContain("overflow-auto");
  });

  it("fits the layout viewport, not the sliver left above a raised soft keyboard", async () => {
    // The fullscreen overlay is `position: fixed` — it keeps covering the whole
    // layout viewport while the soft keyboard shrinks the VISUAL viewport. Fitting
    // to the visual viewport asked for a grid a few rows tall (clamped at
    // MIN_RESIZE_ROWS) that stayed that short after the keyboard went away.
    const originalVisual = Object.getOwnPropertyDescriptor(
      window,
      "visualViewport",
    );
    const originalInner = Object.getOwnPropertyDescriptor(
      window,
      "innerHeight",
    );
    Object.defineProperty(window, "visualViewport", {
      configurable: true,
      value: {
        height: 200,
        addEventListener: vi.fn(),
        removeEventListener: vi.fn(),
      }, // keyboard up
    });
    Object.defineProperty(window, "innerHeight", {
      configurable: true,
      value: 800,
    });
    try {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-phone-kbd",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-phone-kbd"
          viewerIsAuthor
        />,
        {
          route: "/sessions/sess-phone-kbd?fullscreen=1",
        },
      );

      act(() => capturedHandlers?.onFrame(80, 24, new Uint8Array([72, 105])));
      // clientHeight 0 (jsdom lays nothing out) forces the measured fallback.
      measureTerminal({ viewportWidth: 390, viewportHeight: 0 });

      await selectMenuItem(/fit columns and rows to screen/i);

      // 788px of layout viewport ⇒ 40 rows. The visual viewport would have
      // yielded 9, clamped up to the 10-row floor.
      await waitFor(() =>
        expect(mockResizeLive).toHaveBeenCalledWith("sess-phone-kbd", 39, 40),
      );
    } finally {
      if (originalVisual) {
        Object.defineProperty(window, "visualViewport", originalVisual);
      } else {
        delete (window as { visualViewport?: unknown }).visualViewport;
      }
      if (originalInner) {
        Object.defineProperty(window, "innerHeight", originalInner);
      }
    }
  });

  it("claims vertical drags from the browser while fullscreen", () => {
    // The overlay is `position: fixed`, so the page behind it stays scrollable.
    // A phone commits a gesture to that page on the first uncancelled
    // `touchmove` and ignores every `preventDefault` after — which is why the
    // window scrolled and the session didn't. `touch-action` is the only lever
    // that works before the gesture starts; `pan-x` keeps sideways panning of a
    // too-wide grid native.
    const { unmount } = render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-phone-ta",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-phone-ta"
      />,
      {
        route: "/sessions/sess-phone-ta?fullscreen=1",
      },
    );
    expect(screen.getByTestId("live-terminal-screen").style.touchAction).toBe(
      "pan-x",
    );
    unmount();

    // Windowed, the page must keep scrolling under a drag over the card.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-phone-ta-win",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-phone-ta-win"
      />,
    );
    expect(screen.getByTestId("live-terminal-screen").style.touchAction).toBe(
      "",
    );
  });

  it("scrolls the fullscreen terminal on a vertical touch drag", () => {
    // A fitted grid leaves the terminal box nothing to pan, and xterm's own
    // scrollable layer sits under the one a finger lands on — so move xterm's
    // viewport ourselves, a row per 20px notch.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-phone-scroll",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-phone-scroll"
        viewerIsAuthor
      />,
      {
        route: "/sessions/sess-phone-scroll?fullscreen=1",
      },
    );

    act(() => capturedHandlers?.onFrame(80, 24, new Uint8Array([72, 105])));
    measureTerminal({ viewportWidth: 390, viewportHeight: 720 });

    const box = screen.getByTestId("live-terminal-screen");
    fireEvent.touchStart(box, { touches: [{ clientY: 300 }] });
    const moved = fireEvent.touchMove(box, { touches: [{ clientY: 350 }] });
    fireEvent.touchEnd(box, { touches: [] });

    // Dragging DOWN 50px ⇒ two notches back through history…
    expect(mockScrollLines.mock.calls).toEqual([[-1], [-1]]);
    // …and the gesture is ours, so the page stays put. (`fireEvent` returns
    // false once the handler has called `preventDefault`.)
    expect(moved).toBe(false);

    // Dragging back UP scrolls forward again.
    mockScrollLines.mockClear();
    fireEvent.touchStart(box, { touches: [{ clientY: 350 }] });
    fireEvent.touchMove(box, { touches: [{ clientY: 330 }] });
    expect(mockScrollLines.mock.calls).toEqual([[1]]);
  });

  it("keeps a quick phone flick moving, then eases it to a stop", () => {
    const clock = installAnimationFrameClock();
    try {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-phone-momentum",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-phone-momentum"
          viewerIsAuthor
        />,
        {
          route: "/sessions/sess-phone-momentum?fullscreen=1",
        },
      );
      act(() => capturedHandlers?.onFrame(80, 24, new Uint8Array([72, 105])));
      measureTerminal({ viewportWidth: 390, viewportHeight: 720 });

      // Let the component's initial layout frame finish, then isolate scrolling.
      clock.advance();
      mockScrollLines.mockClear();

      const box = screen.getByTestId("live-terminal-screen");
      fireEvent.touchStart(box, { touches: [{ clientY: 300 }] });
      clock.advance();
      fireEvent.touchMove(box, { touches: [{ clientY: 340 }] });
      fireEvent.touchEnd(box, { touches: [] });

      const rowsDuringDrag = mockScrollLines.mock.calls.length;
      expect(rowsDuringDrag).toBe(2);

      // The release does not emit a synchronous burst. Later animation frames
      // continue in the same direction and eventually settle by themselves.
      clock.advance();
      expect(mockScrollLines.mock.calls.length).toBeGreaterThan(rowsDuringDrag);
      expect(mockScrollLines.mock.calls.every(([rows]) => rows === -1)).toBe(
        true,
      );

      let frames = 1;
      while (clock.pending > 0 && frames < 100) {
        clock.advance();
        frames++;
      }
      expect(clock.pending).toBe(0);
      expect(frames).toBeGreaterThan(4);
      expect(frames).toBeLessThan(100);
    } finally {
      clock.restore();
    }
  });

  it("stops phone momentum when a new touch begins", () => {
    const clock = installAnimationFrameClock();
    try {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-phone-momentum-stop",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-phone-momentum-stop"
          viewerIsAuthor
        />,
        {
          route: "/sessions/sess-phone-momentum-stop?fullscreen=1",
        },
      );
      act(() => capturedHandlers?.onFrame(80, 24, new Uint8Array([72, 105])));
      measureTerminal({ viewportWidth: 390, viewportHeight: 720 });
      clock.advance();
      mockScrollLines.mockClear();

      const box = screen.getByTestId("live-terminal-screen");
      fireEvent.touchStart(box, { touches: [{ clientY: 300 }] });
      clock.advance();
      fireEvent.touchMove(box, { touches: [{ clientY: 340 }] });
      fireEvent.touchEnd(box, { touches: [] });
      expect(clock.pending).toBeGreaterThan(0);

      // Putting a finger down again is the phone equivalent of catching a
      // coasting list. No queued frame may move the terminal afterward.
      fireEvent.touchStart(box, { touches: [{ clientY: 340 }] });
      const rowsWhenCaught = mockScrollLines.mock.calls.length;
      expect(clock.pending).toBe(0);
      clock.advance();
      expect(mockScrollLines.mock.calls).toHaveLength(rowsWhenCaught);
    } finally {
      clock.restore();
    }
  });

  it("does not resume old momentum after exiting and re-entering fullscreen", async () => {
    const clock = installAnimationFrameClock();
    try {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "sess-phone-momentum-fullscreen",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="sess-phone-momentum-fullscreen"
          viewerIsAuthor
        />,
        {
          route: "/sessions/sess-phone-momentum-fullscreen?fullscreen=1",
        },
      );
      act(() => capturedHandlers?.onFrame(80, 24, new Uint8Array([72, 105])));
      measureTerminal({ viewportWidth: 390, viewportHeight: 720 });
      clock.advance();
      mockScrollLines.mockClear();

      const box = screen.getByTestId("live-terminal-screen");
      fireEvent.touchStart(box, { touches: [{ clientY: 300 }] });
      clock.advance();
      fireEvent.touchMove(box, { touches: [{ clientY: 340 }] });
      fireEvent.touchEnd(box, { touches: [] });
      const rowsAtRelease = mockScrollLines.mock.calls.length;
      expect(rowsAtRelease).toBe(2);

      // Toggle out and back before the queued momentum frame runs. The first
      // transition must clean up the old gesture; the new overlay starts idle.
      await selectMenuItem(/exit fullscreen/i);
      await selectMenuItem(/^fullscreen$/i);
      for (let frame = 0; frame < 5; frame++) clock.advance();

      expect(mockScrollLines.mock.calls).toHaveLength(rowsAtRelease);
    } finally {
      clock.restore();
    }
  });

  it("sends a drag on an alternate-screen TUI to the session as wheel input", () => {
    // OpenCode draws in the alternate screen and owns its own history —
    // xterm has no local scrollback to move there, so the drag has to reach the
    // remote as the mouse report a trackpad would produce.
    bufferType = "alternate";
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-phone-alt",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-phone-alt"
        viewerIsAuthor
      />,
      {
        route: "/sessions/sess-phone-alt?fullscreen=1",
      },
    );

    act(() => capturedHandlers?.onFrame(80, 24, new Uint8Array([72, 105])));
    const { grid } = measureTerminal({
      viewportWidth: 390,
      viewportHeight: 720,
    });
    const wheels: number[] = [];
    grid.addEventListener("wheel", (e) =>
      wheels.push((e as WheelEvent).deltaY),
    );

    const box = screen.getByTestId("live-terminal-screen");
    fireEvent.touchStart(box, { touches: [{ clientY: 300 }] });
    fireEvent.touchMove(box, { touches: [{ clientY: 350 }] });

    // Dragging DOWN pulls earlier output into view: wheel-UP, twice.
    expect(wheels).toEqual([-20, -20]);
    expect(mockScrollLines).not.toHaveBeenCalled();
  });

  it("pans the box first when the grid is taller than the screen", () => {
    // Unfitted, the rows below the fold are only reachable by moving the box —
    // xterm's viewport is still showing them, they're just off-screen.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-phone-pan",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-phone-pan"
        viewerIsAuthor
      />,
      {
        route: "/sessions/sess-phone-pan?fullscreen=1",
      },
    );

    act(() => capturedHandlers?.onFrame(80, 24, new Uint8Array([72, 105])));
    const { box } = measureTerminal({
      viewportWidth: 390,
      viewportHeight: 400,
      contentHeight: 1200,
    });
    box.scrollTop = 200;

    fireEvent.touchStart(box, { touches: [{ clientY: 300 }] });
    fireEvent.touchMove(box, { touches: [{ clientY: 250 }] });

    // Dragging UP 50px moves the content up: further down the grid.
    expect(box.scrollTop).toBe(250);
    expect(mockScrollLines).not.toHaveBeenCalled();
  });

  it("hands the rest of a drag to the terminal once the box hits its edge", () => {
    // Otherwise an oversized grid can never reach history: the box swallows the
    // whole drag, clamps, and the terminal never moves.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-phone-edge",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-phone-edge"
        viewerIsAuthor
      />,
      {
        route: "/sessions/sess-phone-edge?fullscreen=1",
      },
    );

    act(() => capturedHandlers?.onFrame(80, 24, new Uint8Array([72, 105])));
    const { box } = measureTerminal({
      viewportWidth: 390,
      viewportHeight: 400,
      contentHeight: 1200,
    });
    box.scrollTop = 30; // near the top of an oversized grid

    fireEvent.touchStart(box, { touches: [{ clientY: 300 }] });
    fireEvent.touchMove(box, { touches: [{ clientY: 350 }] });

    // 30px of the 50px drag pans the box to its top edge; the leftover 20px is
    // one notch back into scrollback.
    expect(box.scrollTop).toBe(0);
    expect(mockScrollLines.mock.calls).toEqual([[-1]]);

    // Same at the far edge, dragging the other way.
    mockScrollLines.mockClear();
    box.scrollTop = 770; // 30px shy of the bottom (1200 - 400)
    fireEvent.touchStart(box, { touches: [{ clientX: 100, clientY: 300 }] });
    fireEvent.touchMove(box, { touches: [{ clientX: 100, clientY: 250 }] });

    expect(box.scrollTop).toBe(800);
    expect(mockScrollLines.mock.calls).toEqual([[1]]);
  });

  it("leaves a sideways drag to the browser so a wide grid can be panned", () => {
    // `touch-action: pan-x` only pans if we don't cancel the gesture, so the
    // axis is picked on the first move and a horizontal one stays the
    // browser's.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-phone-panx",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-phone-panx"
        viewerIsAuthor
      />,
      {
        route: "/sessions/sess-phone-panx?fullscreen=1",
      },
    );

    act(() => capturedHandlers?.onFrame(80, 24, new Uint8Array([72, 105])));
    const { box } = measureTerminal({
      viewportWidth: 390,
      viewportHeight: 720,
    });

    fireEvent.touchStart(box, { touches: [{ clientX: 100, clientY: 300 }] });
    // Mostly sideways, with the slight vertical wobble a real finger has.
    const moved = fireEvent.touchMove(box, {
      touches: [{ clientX: 160, clientY: 310 }],
    });

    expect(moved).toBe(true); // uncancelled: the browser pans
    expect(mockScrollLines).not.toHaveBeenCalled();

    // The axis holds for the rest of the gesture, even once it turns vertical.
    const later = fireEvent.touchMove(box, {
      touches: [{ clientX: 160, clientY: 400 }],
    });
    expect(later).toBe(true);
    expect(mockScrollLines).not.toHaveBeenCalled();
  });

  it("leaves a drag over the windowed terminal to the page", () => {
    // Windowed, the terminal is one card on a scrollable page: swallowing the
    // drag would trap the reader on it.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-phone-page",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-phone-page"
        viewerIsAuthor
      />,
    );

    act(() => capturedHandlers?.onFrame(80, 24, new Uint8Array([72, 105])));
    const { grid } = measureTerminal({
      viewportWidth: 390,
      viewportHeight: 720,
    });
    const wheels: number[] = [];
    grid.addEventListener("wheel", (e) =>
      wheels.push((e as WheelEvent).deltaY),
    );

    const box = screen.getByTestId("live-terminal-screen");
    fireEvent.touchStart(box, { touches: [{ clientY: 300 }] });
    const moved = fireEvent.touchMove(box, { touches: [{ clientY: 380 }] });

    expect(wheels).toEqual([]);
    expect(mockScrollLines).not.toHaveBeenCalled();
    expect(moved).toBe(true); // not cancelled: the page keeps the gesture
  });

  it("holds the page still while the fullscreen overlay is up", () => {
    // Same fixed-overlay problem, outside the terminal box: a drag on the
    // header or key bar scrolled the document under the session.
    const { unmount } = render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-phone-lock",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-phone-lock"
      />,
      {
        route: "/sessions/sess-phone-lock?fullscreen=1",
      },
    );
    expect(document.body.style.overflow).toBe("hidden");
    expect(document.documentElement.style.overflow).toBe("hidden");

    // …and restored on the way out, or the rest of the app is left unscrollable.
    unmount();
    expect(document.body.style.overflow).toBe("");
    expect(document.documentElement.style.overflow).toBe("");
  });

  it("does not arm capture on the click that ends a scroll drag", () => {
    // A write sharee, not the author: an author landing in fullscreen has
    // capture auto-armed, which would mask what this asserts.
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-phone-drag-tap",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-phone-drag-tap"
        viewerIsAuthor={false}
        viewerCanWrite
      />,
      { route: "/sessions/sess-phone-drag-tap?fullscreen=1" },
    );

    act(() => capturedHandlers?.onFrame(80, 24, new Uint8Array([72, 105])));
    measureTerminal({ viewportWidth: 390, viewportHeight: 720 });

    const box = screen.getByTestId("live-terminal-screen");
    fireEvent.touchStart(box, { touches: [{ clientY: 300 }] });
    fireEvent.touchMove(box, { touches: [{ clientY: 360 }] });
    fireEvent.touchEnd(box, { touches: [] });
    fireEvent.click(box);

    // The on-screen key bar only renders while capturing.
    expect(
      screen.queryByRole("button", { name: /escape/i }),
    ).not.toBeInTheDocument();

    // A plain tap (no drag) still arms it.
    fireEvent.click(box);
    expect(screen.getByRole("button", { name: /escape/i })).toBeInTheDocument();
  });
});

describe("LiveTerminal file drop", () => {
  const goLive = () =>
    act(() => {
      capturedHandlers?.onReplayComplete?.();
      capturedHandlers?.onFrame(80, 24, makeBytes([79, 75]));
    });

  const makeFile = (name: string, bytes: number[]) => {
    const file = new File([new Uint8Array(bytes)], name);
    Object.defineProperty(file, "arrayBuffer", {
      value: async () => new Uint8Array(bytes).buffer,
    });
    return file;
  };

  const dropOnTerminal = (files: File[]) =>
    fireEvent.drop(screen.getByTestId("live-terminal-screen"), {
      dataTransfer: {
        types: ["Files"],
        files,
        items: files.map(() => ({ kind: "file" })),
        dropEffect: "none",
      },
    });

  it("attaches files for a writer without arming capture, sending no PTY input", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-drop",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-drop"
        viewerCanWrite
      />,
    );
    goLive();

    dropOnTerminal([makeFile("a b.txt", [1, 2]), makeFile("é.png", [3])]);

    await waitFor(() => expect(mockSendFiles).toHaveBeenCalledTimes(1));
    const [id, files] = mockSendFiles.mock.calls[0];
    expect(id).toBe("sess-drop");
    expect(
      (files as { name: string; bytes: Uint8Array }[]).map((f) => [
        f.name,
        [...f.bytes],
      ]),
    ).toEqual([
      ["a b.txt", [1, 2]],
      ["é.png", [3]],
    ]);
    await waitFor(() => expect(mockFocus).toHaveBeenCalled());
    expect(mockSendInput).not.toHaveBeenCalled();
    expect(mockPaste).not.toHaveBeenCalled();
    expect(isCapturing()).toBe(false);
  });

  it("lets the session author drop files", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-drop",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-drop"
        viewerIsAuthor
      />,
    );
    goLive();

    dropOnTerminal([makeFile("a.txt", [1])]);

    await waitFor(() => expect(mockSendFiles).toHaveBeenCalledTimes(1));
  });

  it("refuses a read-only viewer with an explanation but still stops navigation", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-drop",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-drop"
      />,
    );
    goLive();

    const notCancelled = dropOnTerminal([makeFile("a.txt", [1])]);

    expect(notCancelled).toBe(false);
    expect(mockSendFiles).not.toHaveBeenCalled();
    expect(screen.getByText(/read-only access/)).toBeVisible();
  });

  it("refuses when no CLI host is attached", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-drop",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-drop"
        viewerCanWrite
        cliAttached={false}
      />,
    );
    goLive();

    dropOnTerminal([makeFile("a.txt", [1])]);

    expect(mockSendFiles).not.toHaveBeenCalled();
    expect(screen.getByText(/No CLI host is attached/)).toBeVisible();
  });

  it("refuses before the terminal is live", () => {
    holdReplay();
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-drop",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-drop"
        viewerCanWrite
      />,
    );

    dropOnTerminal([makeFile("a.txt", [1])]);

    expect(mockSendFiles).not.toHaveBeenCalled();
    expect(screen.getByText(/isn't live yet/)).toBeVisible();
  });

  it("refuses on an inactive pooled terminal", () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-drop",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-drop"
        viewerCanWrite
        active={false}
      />,
    );
    goLive();

    dropOnTerminal([makeFile("a.txt", [1])]);

    expect(mockSendFiles).not.toHaveBeenCalled();
  });

  it("shows the host's rejection and keeps the prompt untouched", async () => {
    mockSendFiles.mockRejectedValueOnce(
      new HostControlError(
        "The terminal host is busy. Try again in a moment.",
        "failed",
      ),
    );
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-drop",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-drop"
        viewerCanWrite
      />,
    );
    goLive();

    dropOnTerminal([makeFile("a.txt", [1])]);

    expect(
      await screen.findByText(
        "The terminal host is busy. Try again in a moment.",
      ),
    ).toBeVisible();
    expect(mockSendInput).not.toHaveBeenCalled();
    expect(mockFocus).not.toHaveBeenCalled();
  });

  it("ignores a delivery that completes after unmount", async () => {
    let finish!: () => void;
    mockSendFiles.mockReturnValueOnce(
      new Promise((resolve) => {
        finish = () => resolve(undefined);
      }),
    );
    const { unmount } = render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-drop",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-drop"
        viewerCanWrite
      />,
    );
    goLive();

    dropOnTerminal([makeFile("a.txt", [1])]);
    await waitFor(() => expect(mockSendFiles).toHaveBeenCalled());
    const signal = (mockSendFiles.mock.calls[0][2] as { signal: AbortSignal })
      .signal;
    unmount();
    await act(async () => finish());

    expect(signal.aborted).toBe(true);
    expect(mockFocus).not.toHaveBeenCalled();
  });

  it("keeps clipboard image paste working alongside file drops", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "sess-drop",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="sess-drop"
        viewerCanWrite
      />,
    );
    goLive();
    armCapture();

    const image = new File([new Uint8Array([9])], "shot.png", {
      type: "image/png",
    });
    Object.defineProperty(image, "arrayBuffer", {
      value: async () => new Uint8Array([9]).buffer,
    });
    const event = new Event("paste", { bubbles: true, cancelable: true });
    Object.defineProperty(event, "clipboardData", {
      value: {
        items: [{ type: "image/png", getAsFile: () => image }],
      },
    });
    act(() => {
      document.body.dispatchEvent(event);
    });

    await waitFor(() =>
      expect(mockPasteImage).toHaveBeenCalledWith(
        "sess-drop",
        "image/png",
        new Uint8Array([9]),
        {},
        "host",
      ),
    );
    expect(mockSendFiles).not.toHaveBeenCalled();
  });
});

// Two kinds of scaling: "Zoom to fit" changes
// only how big the letters are drawn; "Fit columns and rows to screen" changes
// the session's real grid. Neither may change the other, and the grid action
// reports what the session actually did rather than what was asked.
describe("LiveTerminal scaling controls", () => {
  it("fits the terminal viewport rather than the full window beside the rails", async () => {
    const width = Object.getOwnPropertyDescriptor(window, "innerWidth");
    Object.defineProperty(window, "innerWidth", {
      configurable: true,
      value: 1600,
    });
    try {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "grid-beside-rails",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="grid-beside-rails"
          viewerIsAuthor
        />,
        {
          route: "/sessions/grid-beside-rails?fullscreen=1",
        },
      );
      act(() => capturedHandlers?.onFrame(80, 24, makeBytes([72])));
      const { grid } = measureTerminal({
        viewportWidth: 640,
        viewportHeight: 480,
      });
      Object.defineProperty(grid.parentElement!, "clientWidth", {
        configurable: true,
        value: 1600,
      });
      await fitGridFromMenu();
      await waitFor(() =>
        expect(mockResizeLive).toHaveBeenCalledWith(
          "grid-beside-rails",
          80,
          30,
        ),
      );
    } finally {
      if (width) Object.defineProperty(window, "innerWidth", width);
    }
  });

  it("keeps capture armed and refocuses after selecting portalled zoom", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "captured-menu-zoom",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="captured-menu-zoom"
        viewerIsAuthor
      />,
    );
    armCapture();
    const zoom = await zoomItem();
    mockFocus.mockClear();
    await userEvent.setup().click(zoom);
    expect(isCapturing()).toBe(true);
    await waitFor(() => expect(mockFocus).toHaveBeenCalled());
  });

  it.each(["Display", "Actions"])(
    "keeps fullscreen %s menu navigation out of the terminal",
    async (menu) => {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "keyboard-sizing-menu",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="keyboard-sizing-menu"
          viewerIsAuthor
        />,
        {
          route: "/sessions/keyboard-sizing-menu?fullscreen=1",
        },
      );
      const user = userEvent.setup();
      await user.click(screen.getByRole("button", { name: menu }));
      await screen.findByRole("menu", { name: menu });
      mockSendInput.mockClear();
      await user.keyboard("{ArrowDown}{ArrowUp}{Escape}");
      expect(mockSendInput).not.toHaveBeenCalled();
      expect(screen.queryByRole("menu")).not.toBeInTheDocument();
      expect(isCapturing()).toBe(true);
    },
  );

  it("keeps zoom and grid fitting together in the Display menu on desktop", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "desktop-sizing-menu",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="desktop-sizing-menu"
        viewerIsAuthor
      />,
    );
    expect(
      screen.queryByRole("button", { name: "Zoom to fit" }),
    ).not.toBeInTheDocument();
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "Display" }));
    const zoom = await screen.findByRole("menuitem", { name: "Zoom to fit" });
    expect(
      screen.getByRole("menuitem", { name: "Fit columns and rows to screen" }),
    ).toBeInTheDocument();
    await user.click(zoom);
    await user.click(screen.getByRole("button", { name: "Display" }));
    expect(
      await screen.findByRole("menuitem", { name: "Reset zoom" }),
    ).toBeInTheDocument();
    expect(mockResizeLive).not.toHaveBeenCalled();
  });

  async function fitGridFromMenu() {
    const user = userEvent.setup();
    await openDisplayMenu();
    await user.click(
      await screen.findByRole("menuitem", {
        name: "Fit columns and rows to screen",
      }),
    );
  }

  /** Hold the resize response so the test decides when the host answers. */
  function deferResize() {
    let answer: (value: { delivered: boolean }) => void = () => {};
    mockResizeLive.mockImplementationOnce(
      () => new Promise((resolve) => (answer = resolve)),
    );
    return (delivered: boolean) =>
      act(async () => {
        answer({ delivered });
        await Promise.resolve();
      });
  }

  it("changes the grid without switching zoom on", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "grid-only",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="grid-only"
        viewerIsAuthor
      />,
    );
    act(() => capturedHandlers?.onFrame(80, 24, makeBytes([72])));
    measureTerminal({ viewportWidth: 1000, viewportHeight: 0 });

    await fitGridFromMenu();

    // 8x16 px cells at the 13px base font; 1000px wide, 768 - 12px tall.
    await waitFor(() =>
      expect(mockResizeLive).toHaveBeenCalledWith("grid-only", 125, 47),
    );
    expect(await zoomLabel()).toBe("Zoom to fit");
  });

  it("sizes the grid for the preferred font size", async () => {
    localStorage.setItem("arrakisTerminalFontSize", "26");
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "grid-preferred-font",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="grid-preferred-font"
        viewerIsAuthor
      />,
    );
    act(() => capturedHandlers?.onFrame(80, 24, makeBytes([72])));
    measureTerminal({ viewportWidth: 1000, viewportHeight: 0 });

    await fitGridFromMenu();

    // 8x16 px cells at 13px become 16x32 at 26px; 1000px wide, 768 - 12px tall.
    await waitFor(() =>
      expect(mockResizeLive).toHaveBeenCalledWith(
        "grid-preferred-font",
        62,
        23,
      ),
    );
  });

  it("confirms the new grid once the session redraws at it", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "grid-headless",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="grid-headless"
        viewerIsAuthor
      />,
    );
    act(() => capturedHandlers?.onFrame(80, 24, makeBytes([72])));
    measureTerminal({ viewportWidth: 1000, viewportHeight: 0 });
    const answer = deferResize();

    await fitGridFromMenu();
    await waitFor(() => expect(mockResizeLive).toHaveBeenCalled());
    await answer(true);
    expect(screen.queryByText(/terminal is now/i)).not.toBeInTheDocument();

    act(() => capturedHandlers?.onFrame(125, 47, makeBytes([72])));
    expect(
      await screen.findByText("Terminal is now 125×47."),
    ).toBeInTheDocument();
  });

  it("reports an unconfirmed resize when the session keeps its size", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "grid-attached",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="grid-attached"
        viewerIsAuthor
      />,
    );
    act(() => capturedHandlers?.onFrame(80, 24, makeBytes([72])));
    measureTerminal({ viewportWidth: 1000, viewportHeight: 0 });
    const answer = deferResize();

    await fitGridFromMenu();
    await waitFor(() => expect(mockResizeLive).toHaveBeenCalled());
    vi.useFakeTimers();
    try {
      await answer(true);
      // The local window owns the size: output keeps arriving at 80x24.
      act(() => capturedHandlers?.onFrame(80, 24, makeBytes([73])));
      act(() => vi.advanceTimersByTime(GRID_CONFIRM_TIMEOUT_MS));
    } finally {
      vi.useRealTimers();
    }

    expect(
      screen.getByText(
        "Resize to 125×47 was not confirmed. Current size is 80×24; an attached terminal may control it.",
      ),
    ).toBeInTheDocument();
    expect(screen.queryByText(/asked cli host/i)).not.toBeInTheDocument();
  });

  it("reports a different size the session took as unconfirmed, not success", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "grid-other",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="grid-other"
        viewerIsAuthor
      />,
    );
    act(() => capturedHandlers?.onFrame(80, 24, makeBytes([72])));
    measureTerminal({ viewportWidth: 1000, viewportHeight: 0 });
    const answer = deferResize();

    await fitGridFromMenu();
    await waitFor(() => expect(mockResizeLive).toHaveBeenCalled());
    vi.useFakeTimers();
    try {
      await answer(true);
      // Something else resized the session meanwhile (e.g. its local window).
      act(() => capturedHandlers?.onFrame(90, 30, makeBytes([73])));
      act(() => vi.advanceTimersByTime(GRID_CONFIRM_TIMEOUT_MS));
    } finally {
      vi.useRealTimers();
    }

    expect(
      screen.getByText(
        "Resize to 125×47 was not confirmed. Current size is 90×30.",
      ),
    ).toBeInTheDocument();
    expect(screen.queryByText(/terminal is now/i)).not.toBeInTheDocument();
  });

  it("keeps confirming the latest request when an earlier one fails late", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "grid-twice",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="grid-twice"
        viewerIsAuthor
      />,
    );
    act(() => capturedHandlers?.onFrame(80, 24, makeBytes([72])));
    measureTerminal({ viewportWidth: 1000, viewportHeight: 0 });
    let failFirst: (err: Error) => void = () => {};
    mockResizeLive.mockImplementationOnce(
      () => new Promise((_resolve, reject) => (failFirst = reject)),
    );
    await fitGridFromMenu();
    await waitFor(() => expect(mockResizeLive).toHaveBeenCalledTimes(1));
    const answerSecond = deferResize();
    await fitGridFromMenu();
    await waitFor(() => expect(mockResizeLive).toHaveBeenCalledTimes(2));

    await act(async () => {
      failFirst(new Error("network"));
      await Promise.resolve();
    });
    await answerSecond(true);
    act(() => capturedHandlers?.onFrame(125, 47, makeBytes([72])));

    expect(
      await screen.findByText("Terminal is now 125×47."),
    ).toBeInTheDocument();
    expect(screen.queryByText(/could not resize/i)).not.toBeInTheDocument();
  });

  it("ignores a late answer to an earlier request for the same grid", async () => {
    render(
      <LiveTerminal
        privateHostId="host"
        privateSessions={[
          {
            id: "grid-late-ok",
            agent: "claude",
            cwd: "/work",
            title: null,
            messages: 1,
            last_activity: null,
            running: true,
          },
        ]}
        id="grid-late-ok"
        viewerIsAuthor
      />,
    );
    act(() => capturedHandlers?.onFrame(80, 24, makeBytes([72])));
    measureTerminal({ viewportWidth: 1000, viewportHeight: 0 });
    const answerFirst = deferResize();
    await fitGridFromMenu();
    await waitFor(() => expect(mockResizeLive).toHaveBeenCalledTimes(1));
    const answerSecond = deferResize();
    await fitGridFromMenu();
    await waitFor(() => expect(mockResizeLive).toHaveBeenCalledTimes(2));

    vi.useFakeTimers();
    try {
      // The second request finds no host; the first answers late.
      await answerSecond(false);
      expect(screen.getByText(/no cli host attached/i)).toBeInTheDocument();
      await answerFirst(true);
      act(() => vi.advanceTimersByTime(GRID_CONFIRM_TIMEOUT_MS));
      expect(screen.queryByText(/not confirmed/i)).not.toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it("drops a pending request when the terminal switches session", async () => {
    function Harness() {
      const [id, setId] = React.useState("grid-before");
      return (
        <>
          <button onClick={() => setId("grid-after")}>Switch session</button>
          <LiveTerminal privateHostId="host" id={id} viewerIsAuthor />
        </>
      );
    }
    render(<Harness />);
    act(() => capturedHandlers?.onFrame(80, 24, makeBytes([72])));
    measureTerminal({ viewportWidth: 1000, viewportHeight: 0 });
    const answer = deferResize();
    await fitGridFromMenu();
    await waitFor(() =>
      expect(mockResizeLive).toHaveBeenCalledWith("grid-before", 125, 47),
    );

    act(() => screen.getByRole("button", { name: "Switch session" }).click());
    vi.useFakeTimers();
    try {
      await answer(true);
      // The new session happens to draw at the old request's grid.
      act(() => capturedHandlers?.onFrame(125, 47, makeBytes([72])));
      expect(screen.queryByText(/terminal is now/i)).not.toBeInTheDocument();
      act(() => vi.advanceTimersByTime(GRID_CONFIRM_TIMEOUT_MS));
      expect(screen.queryByText(/not confirmed/i)).not.toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not ask the host when the grid already fits", async () => {
    const inner = Object.getOwnPropertyDescriptor(window, "innerHeight");
    // 24 rows of 16px plus the 12px margin.
    Object.defineProperty(window, "innerHeight", {
      configurable: true,
      value: 400,
    });
    try {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "grid-same",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="grid-same"
          viewerIsAuthor
        />,
      );
      act(() => capturedHandlers?.onFrame(80, 24, makeBytes([72])));
      measureTerminal({ viewportWidth: 640, viewportHeight: 0 });

      await fitGridFromMenu();

      expect(
        await screen.findByText("Terminal already fits this screen at 80×24."),
      ).toBeInTheDocument();
      expect(mockResizeLive).not.toHaveBeenCalled();
    } finally {
      if (inner) Object.defineProperty(window, "innerHeight", inner);
    }
  });

  it("zooms a windowed terminal to fit the window height too", async () => {
    const inner = Object.getOwnPropertyDescriptor(window, "innerHeight");
    Object.defineProperty(window, "innerHeight", {
      configurable: true,
      value: 300,
    });
    try {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "zoom-windowed",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="zoom-windowed"
        />,
      );
      const term = vi.mocked(XtermTerminal).mock.results.at(-1)?.value as {
        element?: HTMLElement;
        options: { fontSize: number };
      };
      const grid = screen.getByTestId("live-terminal-grid");
      Object.defineProperty(term, "element", {
        configurable: true,
        value: grid,
      });
      Object.defineProperty(grid, "offsetWidth", {
        configurable: true,
        get: () => (640 * term.options.fontSize) / 13,
      });
      Object.defineProperty(grid, "offsetHeight", {
        configurable: true,
        get: () => (384 * term.options.fontSize) / 13,
      });
      Object.defineProperty(grid.parentElement!, "clientWidth", {
        configurable: true,
        value: 1600,
      });

      await toggleZoom();

      // The whole grid fits the 288px left in the window, not just its width.
      await waitFor(() =>
        expect(grid.offsetHeight).toBeLessThanOrEqual(300 - 12),
      );
      expect(mockResizeLive).not.toHaveBeenCalled();
    } finally {
      if (inner) Object.defineProperty(window, "innerHeight", inner);
    }
  });

  describe("on a touch screen", () => {
    let original: PropertyDescriptor | undefined;
    beforeEach(() => {
      original = Object.getOwnPropertyDescriptor(navigator, "maxTouchPoints");
      Object.defineProperty(navigator, "maxTouchPoints", {
        configurable: true,
        value: 5,
      });
    });
    afterEach(() => {
      if (original)
        Object.defineProperty(navigator, "maxTouchPoints", original);
      else delete (navigator as { maxTouchPoints?: number }).maxTouchPoints;
    });

    it("sizes the grid for the base font when zoom is off", async () => {
      render(
        <LiveTerminal
          privateHostId="host"
          privateSessions={[
            {
              id: "touch-no-zoom",
              agent: "claude",
              cwd: "/work",
              title: null,
              messages: 1,
              last_activity: null,
              running: true,
            },
          ]}
          id="touch-no-zoom"
          viewerIsAuthor
        />,
        {
          route: "/sessions/touch-no-zoom?fullscreen=1",
        },
      );
      act(() => capturedHandlers?.onFrame(80, 24, makeBytes([72])));
      measureTerminal({ viewportWidth: 390, viewportHeight: 720 });
      const user = userEvent.setup();
      await user.click(screen.getByRole("button", { name: /^actions$/i }));
      await user.click(
        await screen.findByRole("menuitem", { name: "Reset zoom" }),
      );

      await fitGridFromMenu();

      // Zoom off draws at most 13px, so the grid uses 8x16px cells. Sizing it
      // for the 16px zoom font would leave a fifth of the screen empty.
      await waitFor(() =>
        expect(mockResizeLive).toHaveBeenCalledWith("touch-no-zoom", 48, 45),
      );
    });
  });
});

describe("LiveTerminal followed privately from a paired host", () => {
  it("clears the loading spinner as soon as the private subscription is open, even before any frames arrive", async () => {
    // A private host that admits the subscription but has not yet delivered
    // its first screen frame used to leave the viewer stuck on "Loading
    // terminal…" forever: the loading state only cleared when the snapshot
    // path fired `onReplayComplete`, which skipped the subscribe-success
    // signal. Landing directly in fullscreen (`?fullscreen=1`) surfaced this
    // because the viewer came in cold with no cached buffer to show instead.
    mockOpenPrivatePty.mockClear();
    render(<LiveTerminal id="sess-private-loading" privateHostId="host-a" />, {
      route: "/?fullscreen=1",
    });
    await waitFor(() => expect(mockOpenPrivatePty).toHaveBeenCalledTimes(1));
    await waitFor(() =>
      expect(screen.queryByText("Loading terminal…")).not.toBeInTheDocument(),
    );
  });

  it("reads the screen from the private channel only and keeps nothing in browser storage", async () => {
    mockOpenPrivatePty.mockClear();
    mockLoadTerminalSnapshot.mockClear();
    mockSaveTerminalSnapshot.mockClear();
    render(
      <LiveTerminal
        id="sess-private-1"
        privateHostId="host-a"
        viewerCanWrite
      />,
    );
    await waitFor(() => expect(mockOpenPrivatePty).toHaveBeenCalledTimes(1));
    expect(mockOpenPrivatePty.mock.calls[0][0]).toBe("host-a");
    expect(mockOpenPrivatePty.mock.calls[0][1]).toBe("sess-private-1");
    act(() => {
      capturedHandlers?.onFrame(80, 24, makeBytes([104, 105]));
      capturedHandlers?.onReplayComplete?.();
    });
    await waitFor(() =>
      expect(mockWrite).toHaveBeenCalledWith(
        makeBytes([104, 105]),
        expect.any(Function),
      ),
    );
    await new Promise((r) => setTimeout(r, 900));
    expect(mockLoadTerminalSnapshot).not.toHaveBeenCalled();
    expect(mockSaveTerminalSnapshot).not.toHaveBeenCalled();
    // A reconnect never asks the relay for a repaint: the host's snapshot heals.
    expect(mockRepaintLive).not.toHaveBeenCalled();
  });

  it("asks the server nothing about the session and offers only display controls", async () => {
    mockListSessionPrs.mockClear();
    render(
      <LiveTerminal
        id="sess-private-menus"
        privateHostId="host-a"
        viewerCanWrite
      />,
    );
    await waitFor(() => expect(mockOpenPrivatePty).toHaveBeenCalled());
    expect(screen.getByRole("button", { name: "Display" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Actions" })).toBeNull();
    // Diagrams is now a menu item inside Display, not a sibling toolbar button.
    expect(
      screen.queryByRole("button", { name: /terminal diagrams/i }),
    ).toBeNull();
    await openDisplayMenu();
    expect(
      await screen.findByRole("menuitem", { name: /^diagrams$/i }),
    ).toBeInTheDocument();
    expect(mockListSessionPrs).not.toHaveBeenCalled();
  });

  it("pastes a phone clipboard screenshot once to the selected private host without text or Enter", async () => {
    const touch = Object.getOwnPropertyDescriptor(navigator, "maxTouchPoints");
    const clipboard = Object.getOwnPropertyDescriptor(navigator, "clipboard");
    const bytes = new Uint8Array([0x89, 0x50, 0x4e, 0x47]);
    const image = {
      type: "",
      size: bytes.length,
      arrayBuffer: async () => bytes.buffer,
    };
    Object.defineProperty(navigator, "maxTouchPoints", {
      configurable: true,
      value: 5,
    });
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: {
        read: vi
          .fn()
          .mockResolvedValue([
            { types: ["image/png"], getType: async () => image },
          ]),
      },
    });
    try {
      render(
        <LiveTerminal id="phone-image" privateHostId="host-a" viewerIsAuthor />,
      );
      act(() => capturedHandlers?.onFrame(80, 24, makeBytes([1])));
      armCapture();
      await act(async () =>
        fireEvent.click(screen.getByRole("button", { name: "Paste" })),
      );
      expect(mockPasteImage).toHaveBeenCalledExactlyOnceWith(
        "phone-image",
        "image/png",
        bytes,
        {},
        "host-a",
      );
      expect(mockPaste).not.toHaveBeenCalled();
      expect(mockSendInput).not.toHaveBeenCalled();
    } finally {
      if (touch) Object.defineProperty(navigator, "maxTouchPoints", touch);
      else delete (navigator as { maxTouchPoints?: number }).maxTouchPoints;
      if (clipboard) Object.defineProperty(navigator, "clipboard", clipboard);
      else delete (navigator as { clipboard?: Clipboard }).clipboard;
    }
  });

  it("pastes images to the named host", async () => {
    mockPasteImage.mockClear();
    render(
      <LiveTerminal
        id="sess-private-2"
        privateHostId="host-a"
        viewerIsAuthor
      />,
    );
    act(() => capturedHandlers?.onFrame(80, 24, makeBytes([1])));
    armCapture();
    const bytes = new Uint8Array([0x89, 0x50]);
    const file = {
      type: "image/png",
      size: bytes.length,
      arrayBuffer: () => Promise.resolve(bytes.buffer),
    } as unknown as File;
    const ev = new Event("paste", { bubbles: true });
    Object.defineProperty(ev, "clipboardData", {
      value: { items: [{ type: "image/png", getAsFile: () => file }] },
    });
    act(() => {
      document.dispatchEvent(ev);
    });
    await waitFor(() =>
      expect(mockPasteImage).toHaveBeenCalledWith(
        "sess-private-2",
        "image/png",
        bytes,
        {},
        "host-a",
      ),
    );
  });

  function pastePrivateImage(id: string, size = 2) {
    render(<LiveTerminal id={id} privateHostId="host-a" viewerIsAuthor />);
    act(() => capturedHandlers?.onFrame(80, 24, makeBytes([1])));
    armCapture();
    const bytes = new Uint8Array([0x89, 0x50]);
    const file = {
      type: "image/png",
      size,
      arrayBuffer: () => Promise.resolve(bytes.buffer),
    } as unknown as File;
    const ev = new Event("paste", { bubbles: true });
    Object.defineProperty(ev, "clipboardData", {
      value: { items: [{ type: "image/png", getAsFile: () => file }] },
    });
    act(() => {
      document.dispatchEvent(ev);
    });
  }

  it.each([
    [
      "refused",
      new HostControlError(
        "Your host refused the image. Nothing was pasted.",
        "refused",
      ),
      "Your host refused the image. Nothing was pasted.",
    ],
    [
      "uncertain",
      new HostControlError(
        "Your host received it, but the terminal did not confirm the paste. Check the terminal before sending it again.",
        "uncertain",
      ),
      "Your host received it, but the terminal did not confirm the paste. Check the terminal before sending it again.",
    ],
    [
      "failed part-way",
      new HostControlError(
        "The transfer did not finish. Some of it may have been pasted; check the terminal before trying again.",
        "failed",
      ),
      "The transfer did not finish. Some of it may have been pasted; check the terminal before trying again.",
    ],
    [
      "unreachable",
      new HostControlError("no route", "transport"),
      "Could not reach your host privately. The image may or may not have been pasted; check the terminal before pasting it again.",
    ],
  ])(
    "says truthfully when a private image paste is %s, and does not send it again",
    async (_case, error, shown) => {
      mockPasteImage.mockClear();
      mockPasteImage.mockRejectedValueOnce(error);
      pastePrivateImage(`sess-private-paste-${_case.replace(/\W/g, "-")}`);
      expect(await screen.findByText(shown)).toBeVisible();
      if (_case !== "refused") {
        expect(screen.queryByText(/Nothing was pasted/)).toBeNull();
      }
      await new Promise((r) => setTimeout(r, 50));
      expect(mockPasteImage).toHaveBeenCalledTimes(1);
      expect(mockSendFiles).not.toHaveBeenCalled();
    },
  );

  it("refuses an image over the private channel's limit before sending anything", async () => {
    mockPasteImage.mockClear();
    pastePrivateImage("sess-private-paste-big", 9 * 1024 * 1024 + 1);
    expect(
      await screen.findByText(
        "This image is larger than 9 MiB. Nothing was sent.",
      ),
    ).toBeVisible();
    expect(mockPasteImage).not.toHaveBeenCalled();
  });

  it("stops and says why when the host refuses the subscription, without retrying", async () => {
    mockOpenPrivatePty.mockClear();
    mockOpenPrivatePty.mockImplementationOnce((_h, _i, handlers) => {
      capturedHandlers = handlers;
      return makeController(
        Promise.reject(
          new HostControlError("The host refused this: revoked.", "refused"),
        ),
      );
    });
    render(<LiveTerminal id="sess-private-3" privateHostId="host-a" />);
    expect(
      await screen.findByText("The host refused this: revoked."),
    ).toBeVisible();
    await new Promise((r) => setTimeout(r, PTY_RECONNECT_BACKOFF_MS + 200));
    expect(mockOpenPrivatePty).toHaveBeenCalledTimes(1);
  });

  function stopPrivateOnce(error: Error) {
    mockOpenPrivatePty.mockClear();
    mockUnlockAgain.mockClear();
    mockOpenPrivatePty.mockImplementationOnce((_h, _i, handlers) => {
      capturedHandlers = handlers;
      return makeController(Promise.reject(error));
    });
  }

  it("asks for a visible unlock when the host withdraws access, and follows again only after it", async () => {
    stopPrivateOnce(
      new PrivateTerminalEnded(
        "Your host no longer allows this view. Unlock it again to continue.",
        "refused",
        "denied",
      ),
    );
    render(<LiveTerminal id="sess-private-denied" privateHostId="host-a" />);
    const button = await screen.findByRole("button", {
      name: "Unlock and follow again",
    });
    await new Promise((r) => setTimeout(r, PTY_RECONNECT_BACKOFF_MS + 200));
    expect(mockOpenPrivatePty).toHaveBeenCalledTimes(1);
    expect(mockUnlockAgain).not.toHaveBeenCalled();
    await userEvent.setup().click(button);
    await waitFor(() => expect(mockOpenPrivatePty).toHaveBeenCalledTimes(2));
    expect(mockUnlockAgain).toHaveBeenCalledWith("host-a");
    expect(mockOpenPrivatePty.mock.calls[1].slice(0, 2)).toEqual([
      "host-a",
      "sess-private-denied",
    ]);
    expect(
      screen.queryByRole("button", { name: "Unlock and follow again" }),
    ).toBeNull();
  });

  it("stays stopped when the unlock is refused or cancelled", async () => {
    stopPrivateOnce(
      new PrivateTerminalEnded("Access withdrawn.", "refused", "denied"),
    );
    mockUnlockAgain.mockRejectedValueOnce(
      new HostControlError("The passkey prompt was cancelled.", "cancelled"),
    );
    render(<LiveTerminal id="sess-private-cancel" privateHostId="host-a" />);
    await userEvent
      .setup()
      .click(
        await screen.findByRole("button", { name: "Unlock and follow again" }),
      );
    expect(
      await screen.findByText("The passkey prompt was cancelled."),
    ).toBeVisible();
    expect(mockOpenPrivatePty).toHaveBeenCalledTimes(1);
    expect(
      screen.getByRole("button", { name: "Unlock and follow again" }),
    ).toBeEnabled();
  });

  it("offers to follow again without an unlock after a second lapsed lease", async () => {
    stopPrivateOnce(
      new PrivateTerminalEnded(
        "The terminal lease lapsed. Follow it again to continue.",
        "refused",
        "expired",
      ),
    );
    render(<LiveTerminal id="sess-private-expired" privateHostId="host-a" />);
    const button = await screen.findByRole("button", { name: "Follow again" });
    await new Promise((r) => setTimeout(r, PTY_RECONNECT_BACKOFF_MS + 200));
    expect(mockOpenPrivatePty).toHaveBeenCalledTimes(1);
    await userEvent.setup().click(button);
    await waitFor(() => expect(mockOpenPrivatePty).toHaveBeenCalledTimes(2));
    expect(mockUnlockAgain).not.toHaveBeenCalled();
  });

  it("reconnects by itself only after a dropped connection", async () => {
    stopPrivateOnce(
      new PrivateTerminalEnded(
        "The private connection dropped.",
        "transport",
        "connection",
      ),
    );
    render(<LiveTerminal id="sess-private-drop" privateHostId="host-a" />);
    await waitFor(() => expect(mockOpenPrivatePty).toHaveBeenCalledTimes(2), {
      timeout: PTY_RECONNECT_BACKOFF_MS + 1000,
    });
    expect(mockUnlockAgain).not.toHaveBeenCalled();
    expect(screen.queryByRole("button", { name: /follow again/i })).toBeNull();
  });
});

it("shows private active sessions in the shared fullscreen switcher", async () => {
  const sessions = [
    {
      id: "private-one",
      title: "Private Alpha",
      agent: "codex",
      cwd: "/work",
      messages: 1,
      running: true,
      last_activity: null,
    },
    {
      id: "private-two",
      title: "Private Beta",
      agent: "claude",
      cwd: "/work",
      messages: 1,
      running: true,
      last_activity: null,
    },
  ];
  render(
    <LiveTerminal
      id="private-one"
      privateHostId="host-a"
      privateSessions={sessions}
      viewerCanWrite
    />,
    { route: "/private/private-one?fullscreen=1" },
  );
  expect(
    await screen.findByRole("tab", { name: /Private Beta/ }),
  ).toBeInTheDocument();
  expect(
    screen.getByRole("tablist", { name: "Live sessions" }),
  ).toBeInTheDocument();
  expect(
    screen.getByRole("button", { name: "End Private Beta" }),
  ).toBeVisible();
  expect(screen.getByRole("button", { name: "New session" })).toBeVisible();
  expect(screen.getByRole("button", { name: "Display" })).toBeVisible();
  await userEvent.click(screen.getByRole("button", { name: "Actions" }));
  expect(screen.getByRole("menuitem", { name: "End session" })).toBeVisible();
  expect(
    screen.getByRole("menuitem", { name: "Rename session" }),
  ).toBeVisible();
  await userEvent.keyboard("{Escape}");
  await userEvent.click(screen.getByRole("tab", { name: /Private Beta/ }));
  expect(inFullscreen()).toBe(true);
});
