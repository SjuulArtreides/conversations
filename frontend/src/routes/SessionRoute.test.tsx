import * as React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Route, Routes, useLocation, useParams } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { renderWithProviders } from "@/test/utils";
import { RestartSessionButton } from "@/components/HostActions";
import { SessionRoute } from "./SessionRoute";
import { api } from "@/lib/api";
import { hostControl, HostControlError } from "@/lib/hostControl";
import type { LiveSession, Session } from "@/lib/types";

const probes = vi.hoisted(() => ({
  mounts: 0,
  disposed: vi.fn(),
  stored: vi.fn(),
}));
vi.mock("@/lib/useControlAvailable", () => ({
  useControlAvailable: () => ({ available: true, isLoading: false }),
}));
vi.mock("@/lib/api", () => ({
  ApiError: class extends Error {},
  api: {
    liveSessions: vi.fn(),
    liveOrder: vi
      .fn()
      .mockResolvedValue({ revision: 0, session_ids: ["sess-1"] }),
    completeLive: vi.fn(),
    getSession: vi.fn(),
  },
}));
vi.mock("@/lib/hostControl", async () => ({
  ...(await vi.importActual<typeof import("@/lib/hostControl")>(
    "@/lib/hostControl",
  )),
  hostControl: {
    reopenSession: vi.fn(),
    startSession: vi.fn(),
    endSession: vi.fn().mockResolvedValue(undefined),
  },
}));
vi.mock("./LiveSessionDetail", () => ({
  LiveSessionDetail: () => {
    const [instance] = React.useState(() => ++probes.mounts);
    React.useEffect(() => () => probes.disposed(instance), [instance]);
    const location = useLocation();
    return (
      <>
        <div data-testid="terminal">Terminal {instance}</div>
        <div data-testid="url">
          {location.pathname}
          {location.search}
        </div>
        <RestartSessionButton sessionId="sess-1" />
      </>
    );
  },
}));
vi.mock("./SessionDetail", () => ({
  SessionDetail: () => {
    probes.stored();
    const { id = "" } = useParams();
    // Mirrors the app-wide 30s staleTime so a remount reuses cached data.
    const { data } = useQuery({
      queryKey: ["session", id],
      queryFn: () => api.getSession(id),
      staleTime: 30_000,
    });
    return (
      <div>
        Stored transcript
        {data && <span> ({data.message_count} messages)</span>}
      </div>
    );
  },
}));

beforeEach(() => {
  vi.clearAllMocks();
  probes.mounts = 0;
  vi.mocked(api.liveSessions).mockResolvedValue([
    { id: "sess-1" } as LiveSession,
  ]);
  vi.mocked(api.completeLive).mockResolvedValue(undefined);
});

function renderSession(search = "?fullscreen=1") {
  return renderWithProviders(
    <Routes>
      <Route path="/sessions/:id" element={<SessionRoute />} />
    </Routes>,
    { route: `/sessions/sess-1${search}` },
  );
}

describe("restart through the session route", () => {
  it("leaves the stored conversation recoverable when the host cannot reopen", async () => {
    vi.mocked(hostControl.reopenSession).mockRejectedValue(
      new HostControlError("The host refused this: no current grant.", "refused"),
    );
    renderSession();
    await screen.findByText("Terminal 1");
    await userEvent.click(
      screen.getByRole("button", { name: "Restart Session" }),
    );
    await screen.findByText("Stored transcript");
    expect(screen.queryByText("Restarting session…")).not.toBeInTheDocument();
    expect(
      await screen.findByText(/The host refused this: no current grant/),
    ).toBeInTheDocument();
  });

  it("restores the live view and does not reopen when completion fails", async () => {
    vi.mocked(api.completeLive).mockRejectedValue(new Error("Close failed"));
    renderSession();
    await screen.findByText("Terminal 1");
    await userEvent.click(
      screen.getByRole("button", { name: "Restart Session" }),
    );
    await screen.findByText("Close failed");
    await screen.findByText("Terminal 2");
    expect(hostControl.reopenSession).not.toHaveBeenCalled();
    expect(probes.stored).not.toHaveBeenCalled();
  });

  it("disposes the old terminal and mounts a fresh one even on immediate registration", async () => {
    vi.mocked(hostControl.reopenSession).mockResolvedValue({ delivered: true as const });
    renderSession();
    await screen.findByText("Terminal 1");
    await userEvent.click(
      screen.getByRole("button", { name: "Restart Session" }),
    );
    await waitFor(() =>
      expect(screen.getByTestId("terminal")).toHaveTextContent("Terminal 2"),
    );
    expect(probes.disposed).toHaveBeenCalledWith(1);
    expect(probes.stored).not.toHaveBeenCalled();
    expect(screen.getByTestId("url")).toHaveTextContent(
      "/sessions/sess-1?fullscreen=1",
    );
  });

  it("shows restart progress without loading the stored transcript while the host is pending", async () => {
    let deliver!: (result: { delivered: true }) => void;
    vi.mocked(hostControl.reopenSession).mockImplementation(
      () =>
        new Promise((resolve) => {
          deliver = resolve;
        }),
    );
    renderSession();
    await screen.findByText("Terminal 1");
    await userEvent.click(
      screen.getByRole("button", { name: "Restart Session" }),
    );
    await waitFor(() => expect(hostControl.reopenSession).toHaveBeenCalled());
    expect(screen.getByText("Restarting session…")).toBeInTheDocument();
    expect(probes.stored).not.toHaveBeenCalled();
    deliver({ delivered: true });
    await screen.findByText("Terminal 2");
  });
});

describe("graduating from the live view", () => {
  it.each(["?fullscreen=1", ""])(
    "switches to the first remaining live tab and preserves display mode (%j)",
    async (search) => {
      const nextSession = { id: "sess-2" } as LiveSession;
      const lastSession = { id: "sess-3" } as LiveSession;
      vi.mocked(api.liveOrder).mockResolvedValueOnce({
        revision: 0,
        session_ids: ["sess-1", "sess-2", "sess-3"],
      }).mockResolvedValueOnce({
        revision: 0,
        session_ids: ["sess-2", "sess-3"],
      });
      vi.mocked(api.liveSessions)
        .mockResolvedValueOnce([
          { id: "sess-1" } as LiveSession,
          lastSession,
          nextSession,
        ])
        .mockResolvedValue([lastSession, nextSession]);
      const { queryClient } = renderSession(search);
      await screen.findByText("Terminal 1");

      await queryClient.invalidateQueries({ queryKey: ["live-sessions"] });

      await screen.findByText(`/sessions/sess-2${search}`);
      expect(probes.stored).not.toHaveBeenCalled();
    },
  );

  it("refetches the stored session that was cached before the session went live", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      vi.mocked(api.liveSessions)
        .mockResolvedValueOnce([])
        .mockResolvedValueOnce([{ id: "sess-1" } as LiveSession])
        .mockResolvedValue([]);
      vi.mocked(api.getSession)
        .mockResolvedValueOnce({ message_count: 0 } as Session)
        .mockResolvedValue({ message_count: 12 } as Session);
      renderSession();
      await screen.findByText("(0 messages)");
      await vi.advanceTimersByTimeAsync(5000);
      await screen.findByText("Terminal 1");
      await vi.advanceTimersByTimeAsync(5000);
      expect(await screen.findByText("(12 messages)")).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });
});
