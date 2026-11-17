import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import { Route, Routes } from "react-router-dom";
import { renderWithProviders } from "@/test/utils";
import type { Role } from "@/lib/types";

// Deferred live stream — no messages yet. These tests assert the shell wins
// BEFORE metadata lands, so we never let the stream deliver content.
vi.mock("@/lib/useLiveStream", () => ({
  useLiveStream: vi.fn(() => ({
    messages: [],
    outline: { count: 0, lastRole: undefined },
    status: { kind: "streaming" },
  })),
}));


vi.mock("@/components/PrActivityPanel", () => ({
  PrActivityPanel: () => <div data-testid="pr-activity-panel" />,
}));

// The live-sessions poll is hand-driven per test.
vi.mock("@/lib/useLiveSessions", () => ({
  useLiveSessions: vi.fn(),
  LIVE_SESSIONS_QUERY_KEY: ["live-sessions"],
}));

vi.mock("@/lib/api", async () => {
  const actual = await vi.importActual<typeof import("@/lib/api")>("@/lib/api");
  return {
    ...actual,
    api: {
      liveSessions: vi.fn().mockResolvedValue([]),
      liveOrder: vi.fn().mockResolvedValue({ revision: 0, session_ids: [] }),
      getLiveSharing: vi.fn().mockResolvedValue([]),
      setLiveSharing: vi.fn(),
      searchUsers: vi.fn().mockResolvedValue([]),
      completeLive: vi.fn().mockResolvedValue(undefined),
      getUserSettings: vi.fn().mockResolvedValue({
        container_paths: [],
        default_agent: "claude",
      }),
      listSessionPrs: vi.fn().mockResolvedValue([]),
      listSessionLinearDocs: vi.fn().mockResolvedValue([]),
    },
  };
});

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
import { SessionRoute } from "./SessionRoute";

const mockedSessions = vi.mocked(useLiveSessions);
const mockedStream = vi.mocked(useLiveStream);



beforeEach(() => {
  vi.clearAllMocks();
  mockRouteParams.id = "sess-1";
  mockedStream.mockReturnValue({
    messages: [],
    outline: { count: 0, lastRole: undefined as unknown as Role | undefined },
    status: { kind: "streaming" },
  });
});


describe("SessionRoute — fullscreen first paint", () => {
  it("replaces the windowed loading spinner with the fullscreen shell", async () => {
    mockedSessions.mockReturnValue({
      list: [],
      activeIds: new Set<string>(),
      isLoading: true,
    });
    renderWithProviders(
      <Routes>
        <Route path="/sessions/:id" element={<SessionRoute />} />
      </Routes>,
      { route: "/sessions/sess-1?fullscreen=1" },
    );
    expect(screen.getByTestId("fullscreen-pending")).toBeInTheDocument();
    expect(screen.queryByText(/Loading session…/)).toBeNull();
    await waitFor(() =>
      expect(screen.getByTestId("fullscreen-pending")).toBeInTheDocument(),
    );
  });

  it("windowed loads keep the plain spinner", () => {
    mockedSessions.mockReturnValue({
      list: [],
      activeIds: new Set<string>(),
      isLoading: true,
    });
    renderWithProviders(
      <Routes>
        <Route path="/sessions/:id" element={<SessionRoute />} />
      </Routes>,
      { route: "/sessions/sess-1" },
    );
    expect(screen.queryByTestId("fullscreen-pending")).toBeNull();
    expect(screen.getByText(/Loading session…/)).toBeInTheDocument();
  });
});
