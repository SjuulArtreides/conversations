import { afterEach, expect, it, vi } from "vitest";
import { act, screen, waitFor } from "@testing-library/react";
import { renderWithProviders } from "@/test/utils";
import { api, ApiError } from "./api";
import { useLiveSessions } from "./useLiveSessions";
import type { LiveSession } from "./types";

function Probe() {
  const { list } = useLiveSessions();
  return <output data-testid="order">{list.map((s) => s.id).join(",")}</output>;
}
afterEach(() => vi.restoreAllMocks());
const sessions = [
  { id: "newer", started_at: "2026-10-19T10:01:00Z" },
  { id: "new", started_at: "2026-10-19T10:02:00Z" },
  { id: "older", started_at: "2026-10-19T10:00:00Z" },
] as LiveSession[];
it("renders shared order and prepends sessions that raced the order read", async () => {
  vi.spyOn(api, "liveSessions").mockResolvedValue(sessions);
  vi.spyOn(api, "liveOrder").mockResolvedValue({
    revision: 4,
    session_ids: ["newer", "older"],
  });
  renderWithProviders(<Probe />);
  await waitFor(() =>
    expect(screen.getByTestId("order")).toHaveTextContent("new,newer,older"),
  );
});
it("uses stable creation order against an older server without the order endpoint", async () => {
  vi.spyOn(api, "liveSessions").mockResolvedValue(sessions);
  vi.spyOn(api, "liveOrder").mockRejectedValue(new ApiError(404, "not found"));
  renderWithProviders(<Probe />);
  await waitFor(() =>
    expect(screen.getByTestId("order")).toHaveTextContent("new,newer,older"),
  );
});

it("refreshes a new session's attachment within one second", async () => {
  const recent = {
    id: "starting",
    started_at: new Date().toISOString(),
    cli_attached: false,
  } as LiveSession;
  const read = vi
    .spyOn(api, "liveSessions")
    .mockResolvedValueOnce([recent])
    .mockResolvedValue([{ ...recent, cli_attached: true }]);
  vi.spyOn(api, "liveOrder").mockResolvedValue({
    revision: 0,
    session_ids: [],
  });
  function Attachment() {
    const { list } = useLiveSessions();
    return <output>{list[0]?.cli_attached ? "ready" : "waiting"}</output>;
  }
  renderWithProviders(<Attachment />);
  await waitFor(() => expect(screen.getByText("ready")).toBeInTheDocument(), {
    timeout: 1500,
  });
  expect(read).toHaveBeenCalledTimes(2);
});

it.each([
  { label: "attached", attached: true, offset: 0 },
  { label: "established and detached", attached: false, offset: -60_000 },
  { label: "future timestamp", attached: false, offset: 60_000 },
])(
  "keeps five-second polling for $label sessions",
  async ({ attached, offset }) => {
    vi.useFakeTimers();
    try {
      const read = vi.spyOn(api, "liveSessions").mockResolvedValue([
        {
          id: "established",
          started_at: new Date(Date.now() + offset).toISOString(),
          cli_attached: attached,
        } as LiveSession,
      ]);
      vi.spyOn(api, "liveOrder").mockResolvedValue({
        revision: 0,
        session_ids: [],
      });
      renderWithProviders(<Probe />);
      await act(async () => {
        await vi.advanceTimersByTimeAsync(1);
      });
      expect(screen.getByTestId("order")).toHaveTextContent("established");
      expect(read).toHaveBeenCalledTimes(1);
      await act(async () => {
        await vi.advanceTimersByTimeAsync(1000);
      });
      expect(read).toHaveBeenCalledTimes(1);
      await act(async () => {
        await vi.advanceTimersByTimeAsync(4000);
      });
      expect(read).toHaveBeenCalledTimes(2);
    } finally {
      vi.useRealTimers();
    }
  },
);
