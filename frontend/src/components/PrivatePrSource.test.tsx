import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { act, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { SessionPrProvider } from "@/lib/sessionPrSource";
import { PullRequestsCard } from "./PullRequestsCard";
import { PrActivityPanel } from "./PrActivityPanel";
import { hostControl } from "@/lib/hostControl";
import { api } from "@/lib/api";
import { readPrivatePrCache } from "@/lib/privatePrCache";
vi.mock("@/lib/privatePrCache", () => ({
  readPrivatePrCache: vi.fn().mockResolvedValue({
    prs: [
      {
        url: "https://github.com/a/b/pull/1",
        owner: "a",
        repo: "b",
        number: 1,
        status: {
          state: "OPEN",
          title: "Private PR",
          merged: false,
          draft: false,
        },
      },
      {
        url: "https://github.com/acme/platform/pull/1095",
        owner: "acme",
        repo: "platform",
        number: 1095,
        detected: true,
        status: null,
      },
    ],
    comments: [
      {
        owner: "a",
        repo: "b",
        number: 1,
        comment_id: "c",
        kind: "issue",
        author: "reviewer",
        body: "Complete private comment",
        html_url: "https://github.com/a/b/pull/1#comment",
        created_at: "2026-11-05T00:00:00Z",
        ingested_at: "2026-11-05T00:00:00Z",
      },
    ],
  }),
}));
vi.mock("@/lib/api", () => ({
  api: {
    listSessionPrs: vi.fn(),
    listPrComments: vi.fn(),
    syncPrs: vi.fn(),
    unlinkSessionPr: vi.fn(),
  },
}));
vi.mock("@/lib/hostControl", () => ({
  hostControl: {
    privateRead: vi.fn(),
    syncPr: vi.fn(),
    commentOnPr: vi.fn(),
    sendReply: vi.fn(),
  },
  HostControlError: class extends Error {},
}));
beforeEach(() => vi.clearAllMocks());
it("uses shared PR cards/comments with explicit host sync and unlink, never central reads", async () => {
  const operation = vi
    .fn()
    .mockResolvedValue({ status: "executed", detail: null, body: null });
  vi.mocked(hostControl.privateRead).mockImplementation(async (_host, read) =>
    read({ operation } as never),
  );
  renderWithProviders(
    <SessionPrProvider hostId="host-a">
      <PullRequestsCard sessionId="s" />
      <PrActivityPanel id="s" canManage interactive />
    </SessionPrProvider>,
  );
  expect(await screen.findByText("Private PR")).toBeVisible();
  // Auto-detected PRs from the local transcript render alongside manually
  // linked ones, tagged so the user can see where the link came from.
  expect(await screen.findByText("acme/platform#1095")).toBeVisible();
  expect(await screen.findByTestId("pr-auto-detected")).toBeVisible();
  await userEvent.click(screen.getByRole("button", { name: "Sync now" }));
  await waitFor(() =>
    expect(hostControl.syncPr).toHaveBeenCalledWith(
      "s",
      expect.objectContaining({ owner: "a", repo: "b", number: 1 }),
      "host-a",
    ),
  );
  await userEvent.click(screen.getByRole("button", { name: /PR activity/i }));
  expect(await screen.findByText("Complete private comment")).toBeVisible();
  await userEvent.type(
    screen.getByRole("textbox", { name: /reply to pr/i }),
    "Reviewed locally.",
  );
  await userEvent.click(screen.getByRole("button", { name: /^send$/i }));
  await waitFor(() =>
    expect(hostControl.commentOnPr).toHaveBeenCalledWith(
      "s",
      { owner: "a", repo: "b", number: 1, body: "Reviewed locally." },
      "host-a",
    ),
  );
  // Two linked PRs ⇒ two Unlink buttons; assert on the first (manual) row.
  await userEvent.click(screen.getAllByRole("button", { name: "Unlink" })[0]);
  await waitFor(() =>
    expect(operation).toHaveBeenCalledWith("pr.unlink", "s", {
      session: "s",
      owner: "a",
      repo: "b",
      number: 1,
    }),
  );
  expect(api.listSessionPrs).not.toHaveBeenCalled();
  expect(api.listPrComments).not.toHaveBeenCalled();
  expect(api.syncPrs).not.toHaveBeenCalled();
  expect(api.unlinkSessionPr).not.toHaveBeenCalled();
});

afterEach(() => vi.useRealTimers());

it.each(["success", "missing"])(
  "does not poll the central list after %s",
  async (outcome) => {
    vi.useFakeTimers();
    if (outcome === "success")
      vi.mocked(api.listSessionPrs).mockResolvedValue([]);
    else
      vi.mocked(api.listSessionPrs).mockRejectedValue(new Error("not found"));
    const rendered = renderWithProviders(
      <PullRequestsCard sessionId="not-ingested" />,
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(api.listSessionPrs).toHaveBeenCalledTimes(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(90_000);
    });
    expect(api.listSessionPrs).toHaveBeenCalledTimes(1);
    rendered.unmount();
    rendered.queryClient.clear();
  },
);

it("polls a successful private cache and stops after a failure", async () => {
  vi.useFakeTimers();
  const rendered = renderWithProviders(
    <SessionPrProvider hostId="host-a">
      <PullRequestsCard sessionId="s" />
    </SessionPrProvider>,
  );
  await act(async () => {
    await vi.advanceTimersByTimeAsync(0);
  });
  expect(readPrivatePrCache).toHaveBeenCalledTimes(1);
  await act(async () => {
    await vi.advanceTimersByTimeAsync(30_000);
  });
  expect(readPrivatePrCache).toHaveBeenCalledTimes(2);
  vi.mocked(readPrivatePrCache).mockRejectedValueOnce(
    new Error("host unavailable"),
  );
  await act(async () => {
    await vi.advanceTimersByTimeAsync(30_000);
  });
  expect(readPrivatePrCache).toHaveBeenCalledTimes(3);
  await act(async () => {
    await vi.advanceTimersByTimeAsync(90_000);
  });
  expect(readPrivatePrCache).toHaveBeenCalledTimes(3);
  rendered.unmount();
  rendered.queryClient.clear();
});
