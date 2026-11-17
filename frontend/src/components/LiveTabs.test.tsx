import { afterEach, describe, expect, it, vi } from "vitest";
import {
  act,
  fireEvent,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, useQuery } from "@tanstack/react-query";
import { renderWithProviders } from "@/test/utils";
import { api } from "@/lib/api";
import {
  LIVE_SESSIONS_QUERY_KEY,
  useLiveSessions,
} from "@/lib/useLiveSessions";
import type { LiveSession } from "@/lib/types";
import { LiveTabs } from "./LiveTabs";
import { hostControl } from "@/lib/hostControl";

const navigate = vi.fn();
vi.mock("react-router-dom", async () => {
  const actual =
    await vi.importActual<typeof import("react-router-dom")>(
      "react-router-dom",
    );
  return { ...actual, useNavigate: () => navigate };
});

function liveSession(overrides: Partial<LiveSession> = {}): LiveSession {
  return {
    id: "sess-1",
    session_id: "sess-1",
    title: "Session one",
    project_path: null,
    started_at: null,
    last_event_at: null,
    mode: "interactive",
    viewer_is_author: true,
    viewer_can_write: true,
    ...overrides,
  };
}

describe("LiveTabs", () => {
  it("renders nothing when the viewer has no live sessions", () => {
    const { container } = renderWithProviders(
      <LiveTabs sessions={[]} currentId="sess-1" />,
    );
    expect(container.querySelector('[role="tablist"]')).toBeNull();
  });

  it("still renders a single tab — the tab is the session's identity", () => {
    // The old behavior hid a lone tab (nothing to switch to). Under the
    // compact session layout the tab IS the session name + activity dot, so
    // it must stay visible even when it's the only one.
    renderWithProviders(
      <LiveTabs
        sessions={[liveSession({ title: "Only live session" })]}
        currentId="sess-1"
      />,
    );
    expect(
      screen.getByRole("tablist", { name: /live sessions/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("tab", { name: /only live session/i }),
    ).toHaveAttribute("aria-selected", "true");
  });

  it("renders a tab per session and highlights the current one", () => {
    const sessions = [
      liveSession({ id: "a", title: "Alpha" }),
      liveSession({ id: "b", title: "Beta" }),
      liveSession({ id: "c", title: "Gamma" }),
    ];
    renderWithProviders(<LiveTabs sessions={sessions} currentId="b" />);

    const tabs = screen.getAllByRole("tab");
    expect(tabs).toHaveLength(3);

    const current = screen.getByRole("tab", { name: /beta/i });
    expect(current).toHaveAttribute("aria-selected", "true");
    expect(screen.getByRole("tab", { name: /alpha/i })).toHaveAttribute(
      "aria-selected",
      "false",
    );
  });

  it("collapses the live-session list behind a mobile control", async () => {
    const sessions = [
      liveSession({ id: "a", title: "Alpha" }),
      liveSession({ id: "b", title: "Beta" }),
    ];
    const user = userEvent.setup();
    renderWithProviders(<LiveTabs sessions={sessions} currentId="a" />);

    const toggle = screen.getByRole("button", { name: /show live sessions/i });
    const tablist = screen.getByRole("tablist", { name: /live sessions/i });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(tablist).toHaveClass("hidden");

    await user.click(toggle);

    expect(
      screen.getByRole("button", { name: /hide live sessions/i }),
    ).toHaveAttribute("aria-expanded", "true");
    expect(tablist).not.toHaveClass("hidden");
  });

  it("marks Codex and OpenCode tabs with their source glyph", () => {
    const sessions = [
      liveSession({ id: "a", title: "Alpha", agent: "codex" }),
      liveSession({ id: "b", title: "Beta", agent: "opencode" }),
      liveSession({ id: "c", title: "Gamma", agent: "claude" }),
    ];
    renderWithProviders(<LiveTabs sessions={sessions} currentId="a" />);

    const codexTab = screen.getByRole("tab", { name: /Alpha/i });
    const opencodeTab = screen.getByRole("tab", { name: /Beta/i });
    const claudeTab = screen.getByRole("tab", { name: /Gamma/i });
    expect(
      within(codexTab).getByLabelText("Codex session"),
    ).toBeInTheDocument();
    expect(
      within(opencodeTab).getByLabelText("OpenCode session"),
    ).toBeInTheDocument();
    expect(
      within(claudeTab).queryByLabelText(/session$/i),
    ).not.toBeInTheDocument();
  });

  it("carries the working directory as the tab tooltip", () => {
    // The compact session layout removed the big session header, so the
    // working directory is only reachable through the tab's native tooltip —
    // regressing this would silently drop that identity cue.
    renderWithProviders(
      <LiveTabs
        sessions={[
          liveSession({
            id: "a",
            title: "Alpha",
            project_path: "/home/dev/my-repo",
          }),
        ]}
        currentId="a"
      />,
    );
    expect(
      screen
        .getByRole("tab", { name: /alpha/i })
        .getAttribute("title")
        ?.split("\n"),
    ).toEqual(expect.arrayContaining(["Alpha", "/home/dev/my-repo"]));
  });

  it("falls back to the cwd basename then a truncated id when no title", () => {
    const sessions = [
      liveSession({ id: "a", title: "", project_path: "/home/dev/my-repo" }),
      liveSession({
        id: "0123456789abcdef",
        title: "",
        project_path: null,
      }),
    ];
    renderWithProviders(<LiveTabs sessions={sessions} currentId="a" />);

    expect(screen.getByRole("tab", { name: /my-repo/i })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: /01234567…/ })).toBeInTheDocument();
  });

  it("navigates to a tab's live route on click", async () => {
    navigate.mockClear();
    const sessions = [
      liveSession({ id: "a", title: "Alpha" }),
      liveSession({ id: "b", title: "Beta" }),
    ];
    const user = userEvent.setup();
    renderWithProviders(<LiveTabs sessions={sessions} currentId="a" />);

    await user.click(screen.getByRole("tab", { name: /beta/i }));
    expect(navigate).toHaveBeenCalledWith("/sessions/b");
  });

  it("does not navigate when clicking the already-current tab", async () => {
    navigate.mockClear();
    const sessions = [
      liveSession({ id: "a", title: "Alpha" }),
      liveSession({ id: "b", title: "Beta" }),
    ];
    const user = userEvent.setup();
    renderWithProviders(<LiveTabs sessions={sessions} currentId="a" />);

    await user.click(screen.getByRole("tab", { name: /alpha/i }));
    expect(navigate).not.toHaveBeenCalled();
  });

  it("preserves ?fullscreen=1 when switching from a fullscreen URL", async () => {
    navigate.mockClear();
    const sessions = [
      liveSession({ id: "a", title: "Alpha" }),
      liveSession({ id: "b", title: "Beta" }),
    ];
    const user = userEvent.setup();
    renderWithProviders(<LiveTabs sessions={sessions} currentId="a" />, {
      route: "/sessions/a?fullscreen=1",
    });

    await user.click(screen.getByRole("tab", { name: /beta/i }));
    expect(navigate).toHaveBeenCalledWith("/sessions/b?fullscreen=1");
  });

  it("opens rename through the authored tab context menu", async () => {
    const sessions = [
      liveSession({ id: "a", title: "Alpha" }),
      liveSession({ id: "b", title: "Beta" }),
    ];
    renderWithProviders(<LiveTabs sessions={sessions} currentId="a" />);

    fireEvent.contextMenu(screen.getByRole("tab", { name: /beta/i }));
    await userEvent.click(
      screen.getByRole("menuitem", { name: /rename session/i }),
    );

    expect(
      screen.getByRole("dialog", { name: /rename session/i }),
    ).toBeInTheDocument();
    expect(screen.getByLabelText("Session title")).toHaveValue("Beta");
  });

  it("opens sharing for the right-clicked session from its popup", async () => {
    vi.spyOn(api, "getLiveSharing").mockResolvedValue([]);
    const user = userEvent.setup();
    renderWithProviders(
      <LiveTabs
        sessions={[
          liveSession({ id: "a", title: "Alpha" }),
          liveSession({ id: "b", title: "Beta" }),
        ]}
        currentId="a"
      />,
    );
    fireEvent.contextMenu(screen.getByRole("tab", { name: /beta/i }));
    await user.click(screen.getByRole("menuitem", { name: /share session/i }));
    expect(
      screen.getByRole("dialog", { name: /^share session$/i }),
    ).toHaveTextContent("Beta");
    await waitFor(() => expect(api.getLiveSharing).toHaveBeenCalledWith("b"));
    expect(
      screen.queryByRole("dialog", { name: /rename session/i }),
    ).not.toBeInTheDocument();
  });

  it("seeds rename with the tab fallback label when no title exists", async () => {
    const sessions = [
      liveSession({ id: "a", title: "Alpha" }),
      liveSession({
        id: "b",
        title: "",
        project_path: "/home/dev/my-repo",
      }),
    ];
    renderWithProviders(<LiveTabs sessions={sessions} currentId="a" />);

    fireEvent.contextMenu(screen.getByRole("tab", { name: /my-repo/i }));
    await userEvent.click(
      screen.getByRole("menuitem", { name: /rename session/i }),
    );

    expect(screen.getByLabelText("Session title")).toHaveValue("my-repo");
  });

  it.each([
    ["read-only", false],
    ["write", true],
  ])(
    "keeps the browser context menu for a %s shared tab with nothing to offer",
    (_label, canWrite) => {
      const sessions = [
        liveSession({ id: "a", title: "Alpha" }),
        liveSession({
          id: "b",
          title: "Beta",
          viewer_is_author: false,
          viewer_can_write: canWrite,
        }),
      ];
      renderWithProviders(<LiveTabs sessions={sessions} currentId="a" />);

      const allowed = fireEvent.contextMenu(
        screen.getByRole("tab", { name: /beta/i }),
      );

      expect(allowed).toBe(true);
      expect(screen.queryByRole("menu")).not.toBeInTheDocument();
    },
  );

  it("offers owner actions but no grid fit for an authored tab", () => {
    renderWithProviders(
      <LiveTabs
        sessions={[
          liveSession({ id: "a", title: "Alpha" }),
          liveSession({ id: "b", title: "Beta" }),
        ]}
        currentId="a"
      />,
    );
    fireEvent.contextMenu(screen.getByRole("tab", { name: /beta/i }));
    expect(
      screen.getByRole("menuitem", { name: /rename session/i }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("menuitem", { name: /fit columns and rows/i }),
    ).not.toBeInTheDocument();
  });

  it.each(["Rename session", "Share session"])(
    "can reopen the same tab menu after canceling %s",
    async (action) => {
      vi.spyOn(api, "getLiveSharing").mockResolvedValue([]);
      renderWithProviders(
        <LiveTabs
          sessions={[
            liveSession({ id: "a", title: "Alpha" }),
            liveSession({ id: "b", title: "Beta" }),
          ]}
          currentId="a"
        />,
      );
      fireEvent.contextMenu(screen.getByRole("tab", { name: /beta/i }));
      await userEvent.click(screen.getByRole("menuitem", { name: action }));
      await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
      fireEvent.contextMenu(screen.getByRole("tab", { name: /beta/i }));
      expect(
        screen.getByRole("menuitem", { name: action }),
      ).toBeInTheDocument();
    },
  );

  it("labels the first nine tabs with their Ctrl-A index", () => {
    const sessions = [
      liveSession({ id: "a", title: "Alpha" }),
      liveSession({ id: "b", title: "Beta" }),
    ];
    renderWithProviders(<LiveTabs sessions={sessions} currentId="a" />);

    const alpha = screen.getByRole("tab", { name: /alpha/i });
    expect(alpha.querySelector("kbd")).toHaveTextContent("1");
    const beta = screen.getByRole("tab", { name: /beta/i });
    expect(beta.querySelector("kbd")).toHaveTextContent("2");
  });

  describe("close ✕", () => {
    afterEach(() => vi.restoreAllMocks());

    it("ends the session through the host, then closes it, without a confirm", async () => {
      const end = vi
        .spyOn(hostControl, "endSession")
        .mockResolvedValue(undefined);
      const complete = vi
        .spyOn(api, "completeLive")
        .mockResolvedValue(undefined);
      const sessions = [
        liveSession({ id: "a", title: "Alpha" }),
        liveSession({ id: "b", title: "Beta" }),
      ];
      const user = userEvent.setup();
      renderWithProviders(<LiveTabs sessions={sessions} currentId="a" />);

      await user.click(screen.getByRole("button", { name: /end beta/i }));
      await waitFor(() => expect(complete).toHaveBeenCalledWith("b"));
      expect(end).toHaveBeenCalledWith("b");
    });

    it("soft-closes a not-mine session via dismissLive without a confirm", async () => {
      const dismiss = vi
        .spyOn(api, "dismissLive")
        .mockResolvedValue({ session_id: "b" });
      const complete = vi
        .spyOn(api, "completeLive")
        .mockResolvedValue(undefined);
      const sessions = [
        liveSession({ id: "a", title: "Alpha" }),
        liveSession({ id: "b", title: "Beta", viewer_is_author: false }),
      ];
      const user = userEvent.setup();
      renderWithProviders(<LiveTabs sessions={sessions} currentId="a" />);

      // The not-mine tab carries a "Close" ✕, not an "End" one.
      expect(
        screen.queryByRole("button", { name: /end beta/i }),
      ).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: /close beta/i }));
      await waitFor(() => expect(dismiss).toHaveBeenCalledWith("b"));
      expect(complete).not.toHaveBeenCalled();
    });

    it("renders a ✕ on every tab (authored ends, not-mine closes)", () => {
      const sessions = [
        liveSession({ id: "a", title: "Alpha" }),
        liveSession({ id: "b", title: "Beta", viewer_is_author: false }),
      ];
      renderWithProviders(<LiveTabs sessions={sessions} currentId="a" />);

      expect(
        screen.getByRole("button", { name: /end alpha/i }),
      ).toBeInTheDocument();
      expect(
        screen.getByRole("button", { name: /close beta/i }),
      ).toBeInTheDocument();
    });

    it("drops the clicked tab and hops to the right neighbour before the close response resolves", async () => {
      // Regression: closing used to await the host + complete round-trip
      // before removing the tab or navigating, so a slow host left the ✕'d
      // tab lingering at the top of the page while the user waited.
      navigate.mockClear();
      let completeClose: () => void = () => {};
      vi.spyOn(hostControl, "endSession").mockResolvedValue(undefined);
      vi.spyOn(api, "completeLive").mockImplementation(
        () => new Promise<void>((resolve) => (completeClose = resolve)),
      );
      const sessions = [
        liveSession({ id: "a", title: "Alpha" }),
        liveSession({ id: "b", title: "Beta" }),
        liveSession({ id: "c", title: "Gamma" }),
      ];
      vi.spyOn(api, "liveSessions").mockResolvedValue(sessions);
      vi.spyOn(api, "liveOrder").mockResolvedValue({
        revision: 0,
        session_ids: sessions.map((s) => s.id),
      });
      const queryClient = new QueryClient({
        defaultOptions: {
          queries: { retry: false },
          mutations: { retry: false },
        },
      });
      queryClient.setQueryData(LIVE_SESSIONS_QUERY_KEY, sessions);
      function Bar() {
        const { list } = useLiveSessions();
        return <LiveTabs sessions={list} currentId="b" />;
      }
      const user = userEvent.setup();
      renderWithProviders(<Bar />, { queryClient });

      await user.click(screen.getByRole("button", { name: /end beta/i }));

      // Tab goes away before the deferred close resolves.
      await waitFor(() =>
        expect(screen.queryByRole("tab", { name: /beta/i })).toBeNull(),
      );
      expect(screen.getByRole("tab", { name: /alpha/i })).toBeInTheDocument();
      expect(screen.getByRole("tab", { name: /gamma/i })).toBeInTheDocument();
      // Current-tab close hops to the neighbour to the right — before resolve.
      expect(navigate).toHaveBeenCalledWith("/sessions/c");

      act(() => completeClose());
    });

    it("restores the tab when the close call fails", async () => {
      // Regression: a failed close must put the tab back so the user can retry
      // — previously the tab stayed because the invalidation already fired.
      let rejectClose: (reason: Error) => void = () => {};
      vi.spyOn(hostControl, "endSession").mockResolvedValue(undefined);
      vi.spyOn(api, "completeLive").mockImplementation(
        () => new Promise<void>((_, reject) => (rejectClose = reject)),
      );
      const sessions = [
        liveSession({ id: "a", title: "Alpha" }),
        liveSession({ id: "b", title: "Beta" }),
      ];
      const queryClient = new QueryClient({
        defaultOptions: {
          queries: { retry: false },
          mutations: { retry: false },
        },
      });
      queryClient.setQueryData(LIVE_SESSIONS_QUERY_KEY, sessions);
      function Bar() {
        const { list } = useLiveSessions();
        return <LiveTabs sessions={list} currentId="a" />;
      }
      const user = userEvent.setup();
      renderWithProviders(<Bar />, { queryClient });

      await user.click(screen.getByRole("button", { name: /end beta/i }));
      await waitFor(() =>
        expect(screen.queryByRole("tab", { name: /beta/i })).toBeNull(),
      );

      await act(async () => {
        rejectClose(new Error("Could not end the session."));
      });

      expect(
        await screen.findByRole("tab", { name: /beta/i }),
      ).toBeInTheDocument();
      expect(
        await screen.findByText(/could not end the session/i),
      ).toBeInTheDocument();
    });

    it("stays closed even when a concurrent live-sessions fetch returns the just-closed id", async () => {
      // Regression: an older poll that was already in flight when the user
      // clicked ✕ used to resolve a moment later and put the closed tab back
      // for a 5-second blip.
      vi.spyOn(hostControl, "endSession").mockResolvedValue(undefined);
      vi.spyOn(api, "completeLive").mockResolvedValue(undefined);
      vi.spyOn(api, "liveOrder").mockResolvedValue({
        revision: 0,
        session_ids: [],
      });
      const sessions = [
        liveSession({ id: "a", title: "Alpha" }),
        liveSession({ id: "b", title: "Beta" }),
      ];
      // The fetch that arrives after close still returns the closed session.
      vi.spyOn(api, "liveSessions").mockResolvedValue(sessions);
      const queryClient = new QueryClient({
        defaultOptions: {
          queries: { retry: false },
          mutations: { retry: false },
        },
      });
      queryClient.setQueryData(LIVE_SESSIONS_QUERY_KEY, sessions);
      function Bar() {
        const { list } = useLiveSessions();
        return <LiveTabs sessions={list} currentId="a" />;
      }
      const user = userEvent.setup();
      renderWithProviders(<Bar />, { queryClient });

      await user.click(screen.getByRole("button", { name: /end beta/i }));
      await waitFor(() => expect(api.completeLive).toHaveBeenCalledWith("b"));
      // The post-success invalidate refetched api.liveSessions, which still
      // includes "b". The pending-close filter must keep "b" off the bar.
      await waitFor(() => expect(api.liveSessions).toHaveBeenCalled());
      expect(screen.queryByRole("tab", { name: /beta/i })).toBeNull();
    });
  });
});

it("moves a tab with keyboard shortcuts and reports failed saves", async () => {
  const reorder = vi
    .spyOn(api, "reorderLiveSession")
    .mockResolvedValue({ revision: 2, session_ids: ["b", "a"] });
  renderWithProviders(
    <LiveTabs
      sessions={[
        liveSession({ id: "a", title: "Alpha" }),
        liveSession({ id: "b", title: "Beta" }),
      ]}
      currentId="a"
    />,
  );
  fireEvent.keyDown(screen.getByRole("tab", { name: /beta/i }), {
    key: "ArrowLeft",
    altKey: true,
  });
  await waitFor(() => expect(reorder).toHaveBeenCalledWith("b", "a"));
  reorder.mockRejectedValueOnce(new Error("Could not save order"));
  fireEvent.keyDown(screen.getByRole("tab", { name: /alpha/i }), {
    key: "ArrowRight",
    altKey: true,
  });
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Could not save order",
  );
});

it.each(["cancel", "fail", "save"] as const)(
  "previews a wrapped-row drag before release and handles %s",
  async (outcome) => {
    vi.restoreAllMocks();
    let resolve!: (value: { revision: number; session_ids: string[] }) => void;
    let reject!: (error: Error) => void;
    const reorder = vi.spyOn(api, "reorderLiveSession").mockImplementation(
      () =>
        new Promise((yes, no) => {
          resolve = yes;
          reject = no;
        }),
    );
    const sessions = [
      liveSession({ id: "a", title: "Alpha" }),
      liveSession({ id: "b", title: "Beta" }),
      liveSession({ id: "c", title: "Gamma" }),
    ];
    let savedOrder = { revision: 1, session_ids: ["a", "b", "c"] };
    vi.spyOn(api, "liveSessions").mockResolvedValue(sessions);
    vi.spyOn(api, "liveOrder").mockImplementation(async () => savedOrder);
    function Tabs() {
      const { list } = useLiveSessions();
      return <LiveTabs sessions={list} currentId="a" />;
    }
    renderWithProviders(<Tabs />);
    await screen.findByRole("tab", { name: /alpha/i });
    const order = () =>
      screen
        .getAllByRole("tab")
        .map((tab) =>
          ["Alpha", "Beta", "Gamma"].find((title) =>
            tab.textContent?.includes(title),
          ),
        );
    const source = screen.getByRole("tab", { name: /alpha/i }).parentElement!;
    const destination = screen.getByRole("tab", {
      name: /gamma/i,
    }).parentElement!;
    vi.spyOn(destination, "getBoundingClientRect").mockReturnValue({
      left: 0,
      top: 50,
      width: 100,
      height: 30,
    } as DOMRect);
    fireEvent.dragStart(source, { dataTransfer: { setData: vi.fn() } });
    fireEvent(
      destination,
      new MouseEvent("dragover", {
        bubbles: true,
        cancelable: true,
        clientX: 90,
        clientY: 60,
      }),
    );
    expect(order()).toEqual(["Beta", "Gamma", "Alpha"]);
    fireEvent(
      screen.getByRole("tab", { name: /beta/i }).parentElement!,
      new MouseEvent("dragover", {
        bubbles: true,
        cancelable: true,
        clientX: 90,
        clientY: 60,
      }),
    );
    expect(order()).toEqual(["Beta", "Gamma", "Alpha"]);
    expect(reorder).not.toHaveBeenCalled();
    if (outcome === "cancel") {
      fireEvent.dragEnd(source);
      expect(order()).toEqual(["Alpha", "Beta", "Gamma"]);
      expect(reorder).not.toHaveBeenCalled();
      return;
    }
    // Release over either the shifted source or empty space within the strip.
    fireEvent.drop(outcome === "save" ? screen.getByRole("tablist") : source);
    fireEvent.dragEnd(source);
    await waitFor(() => expect(reorder).toHaveBeenCalledWith("a", null));
    expect(order()).toEqual(["Beta", "Gamma", "Alpha"]);
    if (outcome === "fail") {
      reject(new Error("Could not save order"));
      expect(await screen.findByRole("alert")).toHaveTextContent(
        "Could not save order",
      );
      expect(order()).toEqual(["Alpha", "Beta", "Gamma"]);
    } else {
      savedOrder = { revision: 2, session_ids: ["b", "c", "a"] };
      resolve(savedOrder);
      await waitFor(() => expect(source).toHaveAttribute("draggable", "true"));
      expect(order()).toEqual(["Beta", "Gamma", "Alpha"]);
    }
  },
);

it.each([
  ["b", "a", 10, "a"],
  ["a", "b", 90, null],
  ["a", "b", 10, "b"],
])(
  "drags %s onto %s at %s and saves the insertion point",
  async (id, target, clientX, beforeId) => {
    vi.restoreAllMocks();
    const reorder = vi
      .spyOn(api, "reorderLiveSession")
      .mockResolvedValue({ revision: 2, session_ids: ["b", "a"] });
    renderWithProviders(
      <LiveTabs
        sessions={[
          liveSession({ id: "a", title: "Alpha" }),
          liveSession({ id: "b", title: "Beta" }),
        ]}
        currentId="a"
      />,
    );
    expect(
      screen.queryByRole("button", { name: /move .* (left|right)/i }),
    ).not.toBeInTheDocument();
    const tabs = screen.getAllByRole("tab");
    const source = tabs[id === "a" ? 0 : 1].parentElement!;
    const destination = tabs[target === "a" ? 0 : 1].parentElement!;
    vi.spyOn(destination, "getBoundingClientRect").mockReturnValue({
      left: 0,
      width: 100,
    } as DOMRect);
    fireEvent.dragStart(source, { dataTransfer: { setData: vi.fn() } });
    fireEvent(
      destination,
      new MouseEvent("drop", { bubbles: true, cancelable: true, clientX }),
    );
    await waitFor(() => expect(reorder).toHaveBeenCalledWith(id, beforeId));
  },
);

it.each([
  ["recent screen change", true, 14_999, "Status unknown"],
  ["idle screen", true, 15_000, "Status unknown"],
  ["no screen data", true, null, "Status unknown"],
  ["disconnected", false, 0, "Disconnected"],
] as const)(
  "keeps legacy %s independent of completion",
  (_name, attached, age, label) => {
    const now = Date.now();
    const clock = vi.spyOn(Date, "now").mockReturnValue(now);
    try {
      renderWithProviders(
        <LiveTabs
          sessions={[
            liveSession({
              id: "a",
              cli_attached: attached,
              last_pty_at:
                age === null ? null : new Date(now - age).toISOString(),
            }),
            liveSession({ id: "b", cli_attached: false }),
          ]}
          currentId="a"
        />,
      );
      expect(
        within(screen.getAllByRole("tab")[0]).getByTitle(label),
      ).toBeInTheDocument();
    } finally {
      clock.mockRestore();
    }
  },
);

describe("private active-session tabs", () => {
  it("uses the same tabs and keyboard navigation but keeps private routes and fullscreen", async () => {
    const { PrivateSessionTabs } = await import("./LiveTabs");
    const sessions = ["one", "two"].map((id) => ({
      id,
      title: `Private ${id}`,
      agent: "codex",
      cwd: "/work",
      messages: 2,
      running: true,
      last_activity: null,
    }));
    const reorder = vi.spyOn(api, "reorderLiveSession").mockClear();
    renderWithProviders(
      <PrivateSessionTabs sessions={sessions} hostId="host" currentId="one" />,
      { route: "/private/one?fullscreen=1" },
    );
    const tabs = screen.getAllByRole("tab");
    expect(tabs).toHaveLength(2);
    expect(tabs[0]).toHaveAttribute("aria-selected", "true");
    fireEvent.keyDown(tabs[0], { key: "ArrowRight" });
    expect(tabs[1]).toHaveFocus();
    await userEvent.click(tabs[1]);
    expect(navigate).toHaveBeenLastCalledWith("/private/two?fullscreen=1");
    fireEvent.keyDown(tabs[1], { key: "ArrowLeft", altKey: true });
    await waitFor(() =>
      expect(screen.getAllByRole("tab")[0]).toHaveTextContent("Private two"),
    );
    expect(reorder).not.toHaveBeenCalled();
    expect(
      screen.getAllByRole("button", { name: /^End Private/ }),
    ).toHaveLength(2);
    const end = vi.spyOn(hostControl, "endSession").mockResolvedValue();
    fireEvent.contextMenu(screen.getAllByRole("tab")[0], {
      clientX: 30,
      clientY: 40,
    });
    expect(
      screen.getByRole("menuitem", { name: "Restart session" }),
    ).toBeVisible();
    await userEvent.keyboard("{Escape}");
    await userEvent.click(
      screen.getByRole("button", { name: "End Private one" }),
    );
    await waitFor(() => expect(end).toHaveBeenCalledWith("one", "host"));
    // The same Ctrl-A shortcut uses the private route and the displayed order.
    fireEvent.keyDown(document, { key: "a", ctrlKey: true });
    fireEvent.keyDown(document, { key: "1" });
    expect(navigate).toHaveBeenLastCalledWith("/private/two?fullscreen=1");
  });

  it.each([
    ["working", "Working"],
    ["background", "Background work"],
    ["needs_input", "Needs you"],
    ["ready", "Ready"],
    ["unknown", "Status unknown"],
    [undefined, "Status unknown"],
  ] as const)(
    "shows native %s even when the transcript is quiet",
    async (readiness, label) => {
      const { PrivateSessionTabs } = await import("./LiveTabs");
      const session = {
        id: "quiet",
        title: "Quiet private",
        agent: "codex",
        cwd: "/work",
        messages: 2,
        running: true,
        last_activity: "2020-01-01T00:00:00Z",
        readiness,
      };
      renderWithProviders(
        <PrivateSessionTabs
          sessions={[session]}
          hostId="host"
          currentId="quiet"
        />,
      );
      expect(
        within(screen.getByRole("tab")).getByRole("img", { name: label }),
      ).toBeInTheDocument();
    },
  );

  it("removes a private tab immediately and restores it when the host end fails", async () => {
    const { PrivateSessionTabs } = await import("./LiveTabs");
    navigate.mockClear();
    let settle: {
      resolve: () => void;
      reject: (err: Error) => void;
    } = { resolve: () => {}, reject: () => {} };
    vi.spyOn(hostControl, "endSession").mockImplementation(
      () =>
        new Promise<void>((resolve, reject) => {
          settle = { resolve, reject };
        }),
    );
    const sessions = [
      {
        id: "one",
        title: "Private one",
        agent: "codex",
        cwd: "/work",
        messages: 2,
        running: true,
        last_activity: null,
      },
      {
        id: "two",
        title: "Private two",
        agent: "codex",
        cwd: "/work",
        messages: 2,
        running: true,
        last_activity: null,
      },
    ];
    const queryClient = new QueryClient({
      defaultOptions: {
        queries: { retry: false },
        mutations: { retry: false },
      },
    });
    queryClient.setQueryData(["private", "host", "sessions"], sessions);
    queryClient.setQueryData(["private", "host", "active-sessions"], sessions);
    function Bar() {
      // Mirror useHostSessions by pulling from the active-sessions cache —
      // the real parent re-renders from this exact key.
      const { data = [] } = useQuery<typeof sessions>({
        queryKey: ["private", "host", "active-sessions"],
        queryFn: () => Promise.resolve(sessions),
        staleTime: Infinity,
      });
      return (
        <PrivateSessionTabs sessions={data} hostId="host" currentId="one" />
      );
    }
    const user = userEvent.setup();
    renderWithProviders(<Bar />, { route: "/private/one", queryClient });

    await user.click(screen.getByRole("button", { name: "End Private one" }));
    // The tab for "one" goes away now; navigate hops to the neighbour.
    await waitFor(() =>
      expect(screen.queryByRole("tab", { name: /private one/i })).toBeNull(),
    );
    expect(navigate).toHaveBeenCalledWith("/private/two");

    await act(async () => {
      settle.reject(new Error("host rejected the end"));
    });

    // A failed end restores the tab so the user can retry.
    expect(
      await screen.findByRole("tab", { name: /private one/i }),
    ).toBeInTheDocument();
  });

  it("marks a private tab with an accessible lock icon, not an emoji", async () => {
    const { PrivateSessionTabs } = await import("./LiveTabs");
    const sessions = [
      {
        id: "one",
        title: "Private one",
        agent: "codex",
        cwd: "/work",
        messages: 2,
        running: true,
        last_activity: null,
      },
    ];
    renderWithProviders(
      <PrivateSessionTabs sessions={sessions} hostId="host" currentId="one" />,
    );
    const tab = screen.getByRole("tab", { name: /private one/i });
    // Accessible label survives, but the glyph is a real SVG icon — no emoji.
    const marker = within(tab).getByLabelText("Private session");
    expect(marker.tagName.toLowerCase()).toBe("svg");
    expect(marker.textContent ?? "").not.toMatch(/🔒/);
  });
});

it("shows both hosts with colliding session IDs and keeps navigation and ending on the right host", async () => {
  const { PrivateSessionTabs } = await import("./LiveTabs");
  const { activeHost } = await import("@/lib/hostPairing");
  const session = {
    id: "same",
    title: "Laptop task",
    agent: "codex",
    cwd: "/work",
    messages: 2,
    running: true,
    last_activity: null,
  };
  const end = vi.spyOn(hostControl, "endSession").mockResolvedValue();
  const selected = activeHost();
  renderWithProviders(
    <PrivateSessionTabs
      sessions={[session]}
      hostId="laptop"
      hostLabel="Laptop"
      currentId="same"
      otherHosts={[
        {
          hostId: "desktop",
          label: "Desktop",
          sessions: [{ ...session, title: "Desktop task" }],
        },
      ]}
    />,
    { route: "/private/same?fullscreen=1&host=laptop" },
  );
  const tabs = screen.getAllByRole("tab");
  expect(tabs).toHaveLength(2);
  const desktopTab = screen.getByRole("tab", { name: /Desktop task/ });
  expect(screen.getByRole("tab", { name: /Laptop task/ })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  expect(desktopTab).toHaveAttribute("aria-selected", "false");
  expect(within(desktopTab).getByLabelText("Host: Desktop")).toBeVisible();
  await userEvent.click(desktopTab);
  expect(navigate).toHaveBeenLastCalledWith(
    "/private/same?fullscreen=1&host=desktop",
  );
  expect(activeHost()).toEqual(selected);
  await userEvent.click(
    screen.getByRole("button", { name: "End Desktop task" }),
  );
  await waitFor(() => expect(end).toHaveBeenCalledWith("same", "desktop"));
});

it("collects running sessions from other paired hosts even when one host is offline", async () => {
  const { AllHostSessionTabs } = await import("./LiveTabs");
  localStorage.setItem(
    "conversations.hosts",
    JSON.stringify({
      active: "a".repeat(43),
      hosts: [
        { id: "a".repeat(43), label: "Laptop" },
        { id: "b".repeat(43), label: "Desktop" },
        { id: "c".repeat(43), label: "Offline" },
      ],
    }),
  );
  const session = {
    id: "one",
    title: "Local task",
    agent: "codex",
    cwd: "/work",
    messages: 2,
    running: true,
    last_activity: null,
  };
  const read = vi
    .spyOn(hostControl, "privateRead")
    .mockImplementation(async (hostId) => {
      if (hostId === "c".repeat(43)) throw new Error("Offline");
      if (hostId === "a".repeat(43)) return [] as never;
      return [{ ...session, title: "Remote task" }] as never;
    });
  renderWithProviders(
    <AllHostSessionTabs
      sessions={[session]}
      hostId={"a".repeat(43)}
      currentId="one"
    />,
  );
  expect(await screen.findByRole("tab", { name: /Remote task/ })).toBeVisible();
  expect(screen.getByRole("tab", { name: /Local task/ })).toBeVisible();
  expect(read).toHaveBeenCalledWith("b".repeat(43), expect.any(Function), {
    allowUnlock: false,
  });
  expect(read).toHaveBeenCalledWith("c".repeat(43), expect.any(Function), {
    allowUnlock: false,
  });
});
