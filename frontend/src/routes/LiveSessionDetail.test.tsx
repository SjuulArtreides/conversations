import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import type {
  AgentKind,
  LiveMode,
  LiveSession,
  Message,
  Role,
} from "@/lib/types";
import { LiveSessionDetail } from "./LiveSessionDetail";

// The live transcript stream is exercised elsewhere; here we drive it through a
// configurable mock so individual tests can set the streamed messages/status.
// Default: "streaming, no messages" (the common case for most assertions).
vi.mock("@/lib/useLiveStream", () => ({
  useLiveStream: vi.fn(() => ({
    messages: [],
    outline: { count: 0, lastRole: undefined },
    status: { kind: "streaming" },
  })),
}));


vi.mock("@/lib/useLiveSessions", () => ({
  useLiveSessions: vi.fn(),
  LIVE_SESSIONS_QUERY_KEY: ["live-sessions"],
}));

// The PR activity panel has its own dedicated test (PrActivityPanel.test.tsx)
// and polls the API on mount; stub it here so this route test stays focused on
// the reply composer / badge / tabs without a second textarea + Send button.
vi.mock("@/components/PrActivityPanel", () => ({
  PrActivityPanel: () => <div data-testid="pr-activity-panel" />,
}));

vi.mock("@/lib/api", async () => {
  const actual = await vi.importActual<typeof import("@/lib/api")>("@/lib/api");
  return {
    ...actual,
    api: {
      getLiveSharing: vi.fn().mockResolvedValue([]),
      setLiveSharing: vi.fn(),
      searchUsers: vi.fn().mockResolvedValue([]),
      completeLive: vi.fn().mockResolvedValue(undefined),
      // StartNewSessionButton reads the default agent for its dialog.
      getUserSettings: vi.fn().mockResolvedValue({
        container_paths: [],
        default_agent: "claude",
      }),
    },
  };
});

const hostMocks = vi.hoisted(() => ({
  sendReply: vi.fn(),
  endSession: vi.fn(),
}));
vi.mock("@/lib/hostControl", async () => {
  const actual =
    await vi.importActual<typeof import("@/lib/hostControl")>(
      "@/lib/hostControl",
    );
  return {
    ...actual,
    hostControl: { ...actual.hostControl, ...hostMocks },
  };
});

// Drive the route at /live/sess-1.
const mockRouteParams = vi.hoisted(() => ({ id: "sess-1" }));

vi.mock("react-router-dom", async () => {
  const actual =
    await vi.importActual<typeof import("react-router-dom")>(
      "react-router-dom",
    );
  return { ...actual, useParams: () => ({ id: mockRouteParams.id }) };
});

import { useLiveSessions } from "@/lib/useLiveSessions";
import { useLiveStream } from "@/lib/useLiveStream";

const mockedSessions = vi.mocked(useLiveSessions);
const mockedStream = vi.mocked(useLiveStream);

/** Build a minimal rendered Message with the given role. */
function message(role: Role, uuid: string): Message {
  return {
    uuid,
    parent_uuid: null,
    role,
    kind: "text",
    model: null,
    is_sidechain: false,
    timestamp: null,
    blocks: [{ type: "text", text: `${role} message` }],
    text: `${role} message`,
    usage: null,
  };
}

/** Override the streamed messages for a single test (status stays live). */
function streamMessages(messages: Message[]) {
  mockedStream.mockReturnValue({
    messages,
    outline: { count: messages.length, lastRole: messages.at(-1)?.role },
    status: { kind: "streaming" },
  });
}

function liveSession(overrides: Partial<LiveSession> = {}): LiveSession {
  return {
    id: "sess-1",
    session_id: "sess-1",
    title: "My live session",
    project_path: null,
    started_at: null,
    last_event_at: null,
    mode: "interactive",
    viewer_is_author: true,
    viewer_can_write: true,
    ...overrides,
  };
}

/**
 * Set up the live-sessions list with a single session. Defaults make the
 * composer visible (interactive + viewer_can_write) and the viewer the author.
 */
function setup({
  agent,
  mode = "interactive" as LiveMode,
  viewerCanWrite = true,
  viewerIsAuthor = true,
  cliAttached,
}: {
  agent?: AgentKind;
  mode?: LiveMode;
  viewerCanWrite?: boolean;
  viewerIsAuthor?: boolean;
  cliAttached?: boolean;
} = {}) {
  mockedSessions.mockReturnValue({
    list: [
      liveSession({
        ...(agent === undefined ? {} : { agent }),
        mode,
        viewer_can_write: viewerCanWrite,
        viewer_is_author: viewerIsAuthor,
        ...(cliAttached === undefined ? {} : { cli_attached: cliAttached }),
      }),
    ],
    activeIds: new Set(["sess-1"]),
    isLoading: false,
  });
}

beforeEach(() => {
  vi.clearAllMocks();
  mockRouteParams.id = "sess-1";
  // Reset the stream to its default "streaming, no messages" each test, so a
  // per-test `streamMessages(...)` override doesn't leak into the next test.
  streamMessages([]);
});




// The page-level LIVE badge and End-session button were removed with the
// big session header. Live/interrupted state is now carried by the tab's
// activity dot (see LiveTabs.test.tsx), and End session lives in the
// Actions menu (see SessionActionsMenu.test.tsx). Right-clicking a tab
// opens the same menu for keyboard-unavailable surfaces.

describe("LiveSessionDetail live tabs fallback (no terminal)", () => {
  // The live terminal's compact row owns the tabs when the terminal is on
  // screen (see LiveTerminal.test.tsx). This route falls back to its own
  // LiveTabs for view-mode / stored / PTY-ended sessions so the viewer still
  // has a switcher and a tab-rendered session identity.
  it("renders a tab per active live session when the terminal pane is not shown", () => {
    mockedSessions.mockReturnValue({
      list: [
        liveSession({ id: "sess-1", title: "First", mode: "view" }),
        liveSession({ id: "sess-2", title: "Second", mode: "view" }),
      ],
      activeIds: new Set(["sess-1", "sess-2"]),
      isLoading: false,
    });
    renderWithProviders(<LiveSessionDetail />);
    expect(screen.queryByTestId("live-terminal")).not.toBeInTheDocument();

    const tabs = screen.getAllByRole("tab");
    expect(tabs).toHaveLength(2);
    expect(screen.getByRole("tab", { name: /first/i })).toHaveAttribute(
      "aria-selected",
      "true",
    );
  });

});



describe("LiveSessionDetail initial scroll", () => {
  it("does not auto-scroll to the bottom on open, even with messages", () => {
    const scrollTo = vi.spyOn(window, "scrollTo").mockImplementation(() => {});
    setup();
    streamMessages([message("user", "u1"), message("assistant", "a1")]);
    renderWithProviders(<LiveSessionDetail />);
    // The initial content load must leave the view where it is (top), so the
    // sticky-scroll helper must not have been invoked on mount.
    expect(scrollTo).not.toHaveBeenCalled();
    scrollTo.mockRestore();
  });
});

describe("LiveSessionDetail transcript collapse", () => {
  it("collapses the HTML transcript by default", () => {
    setup();
    streamMessages([message("user", "u1"), message("assistant", "a1")]);
    renderWithProviders(<LiveSessionDetail />);
    // The chat bubbles are hidden by default…
    expect(screen.queryByText("assistant message")).not.toBeInTheDocument();
    // …behind a toggle that surfaces the message count.
    expect(
      screen.getByRole("button", { name: /show full conversation \(2\)/i }),
    ).toBeInTheDocument();
  });

  it("reveals the transcript when the toggle is clicked", async () => {
    setup();
    streamMessages([message("user", "u1"), message("assistant", "a1")]);
    const user = userEvent.setup();
    renderWithProviders(<LiveSessionDetail />);
    await user.click(
      screen.getByRole("button", { name: /show full conversation/i }),
    );
    expect(screen.getByText("assistant message")).toBeInTheDocument();
    // The toggle flips to "Hide".
    expect(
      screen.getByRole("button", { name: /hide full conversation/i }),
    ).toBeInTheDocument();
  });
});
