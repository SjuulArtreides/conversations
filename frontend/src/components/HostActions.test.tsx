import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Routes, Route, useLocation } from "react-router-dom";
import { renderWithProviders } from "@/test/utils";
import { HostControlError } from "@/lib/hostControl";
import type { PrivateChannel } from "@/lib/privateChannel";
import { markClosing, clearClosing, privateScope } from "@/lib/pendingClose";
import {
  RestartSessionButton,
  ReopenSessionButton,
  StartNewSessionButton,
  useStartNewSession,
} from "./HostActions";

vi.mock("@/lib/localHost", async () => {
  const actual =
    await vi.importActual<typeof import("@/lib/localHost")>("@/lib/localHost");
  return { ...actual, isLocalHost: vi.fn(), localRequest: vi.fn() };
});

vi.mock("@/lib/useControlAvailable", () => ({
  useControlAvailable: vi.fn(),
  CONTROL_AVAILABLE_QUERY_KEY: ["control-available"],
}));

vi.mock("@/lib/hostControl", async () => {
  const actual =
    await vi.importActual<typeof import("@/lib/hostControl")>(
      "@/lib/hostControl",
    );
  return {
    ...actual,
    hostControl: {
      startSession: vi.fn(),
      reopenSession: vi.fn(),
      endSession: vi.fn().mockResolvedValue(undefined),
      privateRead: vi.fn(),
    },
  };
});

// Mutable state so individual tests can flip the paired-host fixture.
const pairedHostsState: { active: string | null; hosts: { id: string }[] } = {
  active: null,
  hosts: [],
};
vi.mock("@/lib/hostPairing", async () => {
  const actual =
    await vi.importActual<typeof import("@/lib/hostPairing")>(
      "@/lib/hostPairing",
    );
  return { ...actual, usePairedHosts: () => pairedHostsState };
});

vi.mock("@/lib/api", async () => {
  const actual = await vi.importActual<typeof import("@/lib/api")>("@/lib/api");
  return {
    ...actual,
    api: {
      completeLive: vi.fn(),
      liveSessions: vi.fn(),
      getUserSettings: vi.fn(),
    },
  };
});

import { api } from "@/lib/api";
import { isLocalHost, localRequest } from "@/lib/localHost";
import { hostControl } from "@/lib/hostControl";
import { useControlAvailable } from "@/lib/useControlAvailable";

const mockedControl = vi.mocked(useControlAvailable);
const mockedReopen = vi.mocked(hostControl.reopenSession);
const mockedStartNew = vi.mocked(hostControl.startSession);
const mockedPrivateRead = vi.mocked(hostControl.privateRead);
const mockedLiveSessions = vi.mocked(api.liveSessions);
const mockedSettings = vi.mocked(api.getUserSettings);

/** Probe that renders the current pathname so navigation can be asserted. */
function LocationProbe() {
  const location = useLocation();
  return (
    <div data-testid="location">
      {location.pathname}
      {location.search}
      <span data-testid="fit-new-session">
        {(location.state as { fitNewSession?: string } | null)?.fitNewSession}
      </span>
    </div>
  );
}

/** A live-sessions list entry with only the fields the button checks. */
const liveEntry = (id: string) =>
  ({ id }) as Awaited<ReturnType<typeof api.liveSessions>>[number];

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(isLocalHost).mockReturnValue(false);
  mockedLiveSessions.mockResolvedValue([]);
  mockedSettings.mockResolvedValue({
    container_paths: [],
    default_agent: "claude",
  });
  pairedHostsState.active = null;
  pairedHostsState.hosts = [];
});

describe("ReopenSessionButton gating", () => {
  it("is disabled with a host-offline tooltip when the paired host is offline", () => {
    mockedControl.mockReturnValue({ available: false, isLoading: false });
    renderWithProviders(<ReopenSessionButton sessionId="sess-1" />);

    const btn = screen.getByRole("button", { name: /re-open as live/i });
    expect(btn).toBeDisabled();
    // Tooltip lives on the wrapping span (a disabled button has no pointer events).
    expect(
      screen.getByTitle(
        /your paired host is offline — run `arrakis-conversations host` on it/i,
      ),
    ).toBeInTheDocument();
  });

  it("renders a hoverable '?' marker explaining why it's disabled", () => {
    mockedControl.mockReturnValue({ available: false, isLoading: false });
    renderWithProviders(<ReopenSessionButton sessionId="sess-1" />);

    const marker = screen.getByLabelText(
      /your paired host is offline — run `arrakis-conversations host` on it/i,
    );
    expect(marker).toHaveTextContent("?");
    expect(marker).toHaveClass("cursor-help");
  });

  it("is enabled when a host is attached", () => {
    mockedControl.mockReturnValue({ available: true, isLoading: false });
    renderWithProviders(<ReopenSessionButton sessionId="sess-1" />);
    expect(
      screen.getByRole("button", { name: /re-open as live/i }),
    ).toBeEnabled();
  });

  it("opens the fullscreen terminal once the reopened session registers", async () => {
    mockedControl.mockReturnValue({ available: true, isLoading: false });
    mockedReopen.mockResolvedValue({ delivered: true as const });
    // First poll: not yet registered; second poll: it appears.
    mockedLiveSessions
      .mockResolvedValueOnce([])
      .mockResolvedValue([liveEntry("sess-1")]);
    const user = userEvent.setup();
    renderWithProviders(
      <>
        <ReopenSessionButton sessionId="sess-1" />
        <Routes>
          <Route path="*" element={<LocationProbe />} />
        </Routes>
      </>,
    );

    await user.click(screen.getByRole("button", { name: /re-open as live/i }));
    await waitFor(() => expect(mockedReopen).toHaveBeenCalledWith("sess-1"));
    await waitFor(() =>
      expect(screen.getByTestId("location")).toHaveTextContent(
        "/sessions/sess-1?fullscreen=1",
      ),
    );
  });

  it("shows the host's refusal as-is, without a fallback", async () => {
    mockedControl.mockReturnValue({ available: true, isLoading: false });
    mockedReopen.mockRejectedValue(
      new HostControlError(
        "The host refused this: no current grant.",
        "refused",
      ),
    );
    const user = userEvent.setup();
    renderWithProviders(<ReopenSessionButton sessionId="sess-1" />);

    await user.click(screen.getByRole("button", { name: /re-open as live/i }));
    expect(
      await screen.findByText(/the host refused this: no current grant/i),
    ).toBeInTheDocument();
  });
});

describe("StartNewSessionButton", () => {
  it("opens the brand-new session once the host registers it", async () => {
    mockedControl.mockReturnValue({ available: true, isLoading: false });
    mockedStartNew.mockResolvedValue({ delivered: true, sessionId: "new-1" });
    // The browser named the session; first poll: not registered yet, then it is.
    mockedLiveSessions
      .mockResolvedValueOnce([])
      .mockResolvedValue([liveEntry("new-1")]);
    const user = userEvent.setup();
    renderWithProviders(
      <>
        <StartNewSessionButton />
        <Routes>
          <Route path="*" element={<LocationProbe />} />
        </Routes>
      </>,
    );

    await user.click(
      screen.getByRole("button", { name: /start new session/i }),
    );
    await waitFor(() =>
      expect(mockedStartNew).toHaveBeenCalledWith("claude", undefined),
    );
    await waitFor(() =>
      expect(screen.getByTestId("location")).toHaveTextContent(
        "/sessions/new-1",
      ),
    );
    expect(screen.getByTestId("fit-new-session")).toHaveTextContent("new-1");
  });

  // A private launch must become usable without waiting for the central live index.
  it("uses the paired host's private channel and navigates to /private/<id>", async () => {
    pairedHostsState.active = "host-abc";
    pairedHostsState.hosts = [{ id: "host-abc" }];
    mockedControl.mockReturnValue({ available: true, isLoading: false });
    mockedStartNew.mockResolvedValue({ delivered: true, sessionId: "new-2" });
    const sessionsList = vi.fn().mockResolvedValue({
      status: "executed",
      detail: null,
      body: {
        kind: "sessions",
        sessions: [
          { id: "new-2", running: true },
          { id: "closing", running: true },
        ],
        next: null,
      },
    });
    mockedPrivateRead.mockImplementation(async (_host, read) =>
      read({ sessionsList } as unknown as PrivateChannel),
    );
    markClosing(privateScope("host-abc"), "closing");
    const user = userEvent.setup();
    const { queryClient } = renderWithProviders(
      <>
        <StartNewSessionButton />
        <Routes>
          <Route path="*" element={<LocationProbe />} />
        </Routes>
      </>,
    );

    await user.click(
      screen.getByRole("button", { name: /start new session/i }),
    );
    await waitFor(() =>
      expect(screen.getByTestId("location")).toHaveTextContent(
        "/private/new-2",
      ),
    );
    expect(mockedPrivateRead).toHaveBeenCalledWith(
      "host-abc",
      expect.any(Function),
    );
    expect(mockedLiveSessions).not.toHaveBeenCalled();
    clearClosing(privateScope("host-abc"), "closing");
    expect(sessionsList).toHaveBeenCalledWith(null, undefined, true);
    expect(
      queryClient.getQueryData(["private", "host-abc", "active-sessions"]),
    ).toEqual([{ id: "new-2", running: true }]);
  });

  it("ends readiness waiting even when a host read never answers", async () => {
    pairedHostsState.active = "host-stalled";
    pairedHostsState.hosts = [{ id: "host-stalled" }];
    mockedControl.mockReturnValue({ available: true, isLoading: false });
    mockedStartNew.mockResolvedValue({
      delivered: true,
      sessionId: "new-stalled",
    });
    mockedPrivateRead.mockImplementation(() => new Promise(() => {}));
    const user = userEvent.setup();
    renderWithProviders(<StartNewSessionButton />);
    await user.click(
      screen.getByRole("button", { name: /start new session/i }),
    );
    await waitFor(
      () =>
        expect(
          screen.getByText(/host accepted the signed request/),
        ).toBeInTheDocument(),
      { timeout: 17_000 },
    );
    expect(mockedStartNew).toHaveBeenCalledTimes(1);
    expect(
      screen.getByRole("button", { name: /start new session/i }),
    ).not.toBeDisabled();
  }, 20_000);

  it("recovers from a transient readiness read without starting another session", async () => {
    pairedHostsState.active = "host-retry";
    pairedHostsState.hosts = [{ id: "host-retry" }];
    mockedControl.mockReturnValue({ available: true, isLoading: false });
    mockedStartNew.mockResolvedValue({
      delivered: true,
      sessionId: "new-retry",
    });
    mockedPrivateRead
      .mockRejectedValueOnce(
        new HostControlError("Connection interrupted", "transport"),
      )
      .mockResolvedValue([{ id: "new-retry", running: true }]);
    const user = userEvent.setup();
    renderWithProviders(
      <>
        <StartNewSessionButton />
        <Routes>
          <Route path="*" element={<LocationProbe />} />
        </Routes>
      </>,
    );
    await user.click(
      screen.getByRole("button", { name: /start new session/i }),
    );
    await waitFor(
      () =>
        expect(screen.getByTestId("location")).toHaveTextContent(
          "/private/new-retry",
        ),
      { timeout: 2000 },
    );
    expect(mockedStartNew).toHaveBeenCalledTimes(1);
    expect(mockedPrivateRead).toHaveBeenCalledTimes(2);
  });

  it("keeps the new session when an older active-session fetch resolves late", async () => {
    pairedHostsState.active = "host-race";
    pairedHostsState.hosts = [{ id: "host-race" }];
    mockedControl.mockReturnValue({ available: true, isLoading: false });
    mockedStartNew.mockResolvedValue({
      delivered: true,
      sessionId: "new-race",
    });
    const fresh = [{ id: "new-race", running: true }];
    mockedPrivateRead.mockResolvedValue(fresh);
    const user = userEvent.setup();
    const { queryClient } = renderWithProviders(
      <>
        <StartNewSessionButton />
        <Routes>
          <Route path="*" element={<LocationProbe />} />
        </Routes>
      </>,
    );
    const key = ["private", "host-race", "active-sessions"];
    let release!: () => void;
    const oldResult = new Promise<typeof fresh>((resolve) => {
      release = () => resolve([]);
    });
    const oldFetch = queryClient
      .fetchQuery({ queryKey: key, queryFn: () => oldResult })
      .catch(() => undefined);
    await user.click(
      screen.getByRole("button", { name: /start new session/i }),
    );
    await waitFor(() =>
      expect(screen.getByTestId("location")).toHaveTextContent(
        "/private/new-race",
      ),
    );
    release();
    await oldResult;
    await oldFetch;
    expect(queryClient.getQueryData(key)).toEqual(fresh);
    expect(
      queryClient.getQueryData(["private", "host-race", "sessions"]),
    ).toEqual(fresh);
  });

  it("shows the host's refusal as-is, without a fallback", async () => {
    mockedControl.mockReturnValue({ available: true, isLoading: false });
    mockedStartNew.mockRejectedValue(
      new HostControlError(
        "Pair a host in Settings first: no host is trusted by this browser.",
        "not_paired",
      ),
    );
    const user = userEvent.setup();
    renderWithProviders(<StartNewSessionButton />);

    await user.click(
      screen.getByRole("button", { name: /start new session/i }),
    );
    expect(
      await screen.findByText(
        /pair a host in settings first.*settings → hosts/i,
      ),
    ).toBeInTheDocument();
  });

  it("is disabled when no host is attached", () => {
    mockedControl.mockReturnValue({ available: false, isLoading: false });
    renderWithProviders(<StartNewSessionButton />);
    expect(
      screen.getByRole("button", { name: /start new session/i }),
    ).toBeDisabled();
  });

  it("can start the non-default agent from the menu", async () => {
    mockedControl.mockReturnValue({ available: true, isLoading: false });
    mockedStartNew.mockRejectedValue(new Error("stop"));
    const user = userEvent.setup();
    renderWithProviders(<StartNewSessionButton />);

    await user.click(screen.getByRole("button", { name: /start with codex/i }));
    await user.click(screen.getByText("Start with Codex"));

    await waitFor(() =>
      expect(mockedStartNew).toHaveBeenCalledWith("codex", undefined),
    );
  });

  it("can start OpenCode through the adk code launcher from the menu", async () => {
    mockedControl.mockReturnValue({ available: true, isLoading: false });
    mockedStartNew.mockRejectedValue(new Error("stop"));
    const user = userEvent.setup();
    renderWithProviders(<StartNewSessionButton />);

    await user.click(screen.getByRole("button", { name: /start with codex/i }));
    await user.click(screen.getByText("Start with adk code"));

    // Same agent as plain OpenCode — only the launcher differs, so the session
    // it produces stays an OpenCode session.
    await waitFor(() =>
      expect(mockedStartNew).toHaveBeenCalledWith("opencode", "adk"),
    );
  });
});

describe("RestartSessionButton", () => {
  it("requires a CLI host", () => {
    mockedControl.mockReturnValue({ available: false, isLoading: false });
    renderWithProviders(<RestartSessionButton sessionId="sess-1" />);
    expect(
      screen.getByRole("button", { name: "Restart Session" }),
    ).toBeDisabled();
  });
  it("closes before resuming the same session", async () => {
    mockedControl.mockReturnValue({ available: true, isLoading: false });
    vi.mocked(api.completeLive).mockResolvedValue(undefined);
    mockedReopen.mockImplementation(async () => {
      // The signed end stops the process first; only then does the server close it.
      expect(hostControl.endSession).toHaveBeenCalledWith("sess-1");
      expect(api.completeLive).toHaveBeenCalledWith("sess-1");
      return { delivered: true as const };
    });
    mockedLiveSessions.mockResolvedValue([liveEntry("sess-1")]);
    renderWithProviders(<RestartSessionButton sessionId="sess-1" />);
    await userEvent.click(
      screen.getByRole("button", { name: "Restart Session" }),
    );
    await waitFor(() => expect(mockedReopen).toHaveBeenCalledWith("sess-1"));
    await waitFor(() => expect(mockedLiveSessions).toHaveBeenCalled());
  });
  it("does not close or reopen when the host refuses to end the session", async () => {
    mockedControl.mockReturnValue({ available: true, isLoading: false });
    vi.mocked(hostControl.endSession).mockRejectedValueOnce(
      new HostControlError(
        "The host refused this: no current grant.",
        "refused",
      ),
    );
    renderWithProviders(<RestartSessionButton sessionId="sess-1" />);
    await userEvent.click(
      screen.getByRole("button", { name: "Restart Session" }),
    );
    expect(await screen.findByText(/no current grant/)).toBeInTheDocument();
    expect(api.completeLive).not.toHaveBeenCalled();
    expect(mockedReopen).not.toHaveBeenCalled();
  });
  it("does not reopen when closing fails", async () => {
    mockedControl.mockReturnValue({ available: true, isLoading: false });
    vi.mocked(api.completeLive).mockRejectedValue(new Error("Close failed"));
    renderWithProviders(<RestartSessionButton sessionId="sess-1" />);
    await userEvent.click(
      screen.getByRole("button", { name: "Restart Session" }),
    );
    expect(await screen.findByText("Close failed")).toBeInTheDocument();
    expect(mockedReopen).not.toHaveBeenCalled();
  });
});

it("sends no launcher of its own: the host applies its local default", async () => {
  mockedControl.mockReturnValue({ available: true, isLoading: false });
  mockedSettings.mockResolvedValue({
    container_paths: [],
    default_agent: "opencode",
  });
  mockedStartNew.mockRejectedValue(new Error("stop"));
  const user = userEvent.setup();
  renderWithProviders(<StartNewSessionButton />);
  await waitFor(() => expect(mockedSettings).toHaveBeenCalled());
  await user.click(screen.getByRole("button", { name: /start new session/i }));
  await waitFor(() =>
    expect(mockedStartNew).toHaveBeenCalledWith("opencode", undefined),
  );
});

function ShortcutStart() {
  const { start } = useStartNewSession();
  return <button onClick={() => void start()}>Shortcut start</button>;
}

it.each(["button", "shortcut"])(
  "reads the saved local default before a %s start",
  async (kind) => {
    vi.mocked(isLocalHost).mockReturnValue(true);
    mockedControl.mockReturnValue({ available: true, isLoading: false });
    let resolve!: (value: {
      container_paths: string[];
      default_agent: "opencode";
    }) => void;
    vi.mocked(localRequest).mockImplementation(
      () =>
        new Promise((done) => {
          resolve = done;
        }),
    );
    mockedStartNew.mockRejectedValue(
      new Error("stop after checking the chosen target"),
    );
    renderWithProviders(
      kind === "button" ? <StartNewSessionButton /> : <ShortcutStart />,
    );
    await userEvent.click(
      screen.getByRole("button", {
        name: kind === "button" ? "Start new session" : "Shortcut start",
      }),
    );
    expect(mockedStartNew).not.toHaveBeenCalled();
    await waitFor(() =>
      expect(localRequest).toHaveBeenCalledWith("/api/local/preferences"),
    );
    resolve({ container_paths: [], default_agent: "opencode" });
    await waitFor(() =>
      expect(mockedStartNew).toHaveBeenCalledWith("opencode", undefined),
    );
    expect(mockedSettings).not.toHaveBeenCalled();
  },
);
