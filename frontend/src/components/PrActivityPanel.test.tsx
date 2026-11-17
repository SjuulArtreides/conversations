import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  fireEvent,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders as renderPanel } from "@/test/utils";
import type { PrComment } from "@/lib/types";
import type { ReactNode } from "react";

vi.mock("@/lib/hostControl", async () => ({
  ...(await vi.importActual<typeof import("@/lib/hostControl")>(
    "@/lib/hostControl",
  )),
  hostControl: { sendReply: vi.fn() },
}));

vi.mock("@/lib/api", () => ({
  ApiError: class ApiError extends Error {
    status: number;
    constructor(status: number, message: string) {
      super(message);
      this.status = status;
    }
  },
  api: {
    listPrComments: vi.fn(),
    replyToPr: vi.fn(),
    syncPrs: vi.fn(),
  },
}));

import { api } from "@/lib/api";
import { hostControl } from "@/lib/hostControl";
import { PrActivityPanel } from "./PrActivityPanel";

function renderWithProviders(ui: ReactNode) {
  const rendered = renderPanel(ui);
  fireEvent.click(screen.getByRole("button", { name: /pr activity/i }));
  return rendered;
}

const comment: PrComment = {
  owner: "acme",
  repo: "widgets",
  number: 42,
  comment_id: "c1",
  kind: "review",
  author: "reviewer-bob",
  body: "Please rename this function.",
  html_url: "https://github.com/acme/widgets/pull/42#discussion_r1",
  created_at: "2026-06-07T10:00:00Z",
  ingested_at: "2026-06-07T10:00:05Z",
};

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(api.listPrComments).mockResolvedValue([comment]);
  vi.mocked(hostControl.sendReply).mockResolvedValue(undefined);
  vi.mocked(api.replyToPr).mockResolvedValue({ delivered: true });
  vi.mocked(api.syncPrs).mockResolvedValue({ delivered: true });
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("PrActivityPanel", () => {
  it("does not fetch full comment bodies until expanded", async () => {
    const user = userEvent.setup();
    renderPanel(<PrActivityPanel id="s1" viewerIsAuthor={false} />);
    expect(api.listPrComments).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: /pr activity/i }));
    expect(await screen.findByText("reviewer-bob")).toBeInTheDocument();
    expect(api.listPrComments).toHaveBeenCalledTimes(1);
  });
  it("renders recent comments first and only expands older Markdown on request", async () => {
    vi.mocked(api.listPrComments).mockResolvedValue(
      Array.from({ length: 30 }, (_, index) => ({
        ...comment,
        comment_id: String(index),
        body: `comment ${index}`,
        created_at: `2026-06-07T10:00:${String(index).padStart(2, "0")}Z`,
      })),
    );
    const user = userEvent.setup();
    renderWithProviders(<PrActivityPanel id="s1" viewerIsAuthor={false} />);
    expect(await screen.findByText("comment 29")).toBeInTheDocument();
    expect(screen.queryByText("comment 0")).not.toBeInTheDocument();
    await user.click(
      screen.getByRole("button", { name: /show more comments/i }),
    );
    expect(screen.getByText("comment 0")).toBeInTheDocument();
  });
  it("shows new discussion even when open reviews fill the initial limit", async () => {
    vi.mocked(api.listPrComments).mockResolvedValue([
      ...Array.from({ length: 25 }, (_, index) => ({
        ...comment,
        kind: "review_comment" as const,
        resolved: false,
        comment_id: `review-${index}`,
        body: `open review ${index}`,
        created_at: `2026-06-07T10:00:${String(index).padStart(2, "0")}Z`,
      })),
      {
        ...comment,
        comment_id: "discussion-new",
        body: "new discussion from reviewer",
        created_at: "2026-06-07T10:01:00Z",
      },
    ]);
    renderWithProviders(<PrActivityPanel id="s1" viewerIsAuthor={false} />);
    expect(
      await screen.findByText("new discussion from reviewer"),
    ).toBeInTheDocument();
    expect(screen.queryByText("open review 0")).not.toBeInTheDocument();
  });
  it("renders fetched comments (author, kind, body, link)", async () => {
    renderWithProviders(<PrActivityPanel id="s1" viewerIsAuthor={false} />);

    expect(await screen.findByText("reviewer-bob")).toBeInTheDocument();
    expect(screen.getByText("review")).toBeInTheDocument();
    expect(
      screen.getByText("Please rename this function."),
    ).toBeInTheDocument();
    const link = screen.getByRole("link", { name: /view on github/i });
    expect(link).toHaveAttribute("href", comment.html_url);
    expect(api.listPrComments).toHaveBeenCalledWith("s1");
  });

  it("surfaces open review comments with file:line + diff and hides resolved ones", async () => {
    const open: PrComment = {
      ...comment,
      comment_id: "rc-open",
      kind: "review_comment",
      body: "This assertion is off by one.",
      path: "src/main.rs",
      line: 41,
      diff_hunk: "@@ -38,4 +38,4 @@\n-old\n+new",
      resolved: false,
    };
    const resolved: PrComment = {
      ...comment,
      comment_id: "rc-done",
      kind: "review_comment",
      body: "already handled, thanks",
      path: "src/lib.rs",
      line: 5,
      resolved: true,
    };
    vi.mocked(api.listPrComments).mockResolvedValue([open, resolved]);
    renderWithProviders(<PrActivityPanel id="s1" viewerIsAuthor={false} />);

    // Open one renders with its location + body + a section header.
    expect(
      await screen.findByText("This assertion is off by one."),
    ).toBeInTheDocument();
    expect(screen.getByText(/open review comments/i)).toBeInTheDocument();
    expect(screen.getByText("src/main.rs:41")).toBeInTheDocument();
    expect(screen.getByText(/diff context/i)).toBeInTheDocument();

    // The resolved review comment is hidden entirely.
    expect(
      screen.queryByText("already handled, thanks"),
    ).not.toBeInTheDocument();
  });

  it("renders embedded GitHub HTML and strips HTML-comment noise", async () => {
    const noisy: PrComment = {
      ...comment,
      comment_id: "c-html",
      kind: "review",
      body:
        "<!-- internal state SECRETNOISE -->\n" +
        "<details><summary>Linked issue</summary>\n\n" +
        "<p>Visible body text.</p></details>",
    };
    vi.mocked(api.listPrComments).mockResolvedValue([noisy]);
    renderWithProviders(<PrActivityPanel id="s1" viewerIsAuthor={false} />);

    // <details>/<summary> render as real elements, not literal "<summary>" text.
    expect(await screen.findByText("Linked issue")).toBeInTheDocument();
    expect(screen.getByText("Visible body text.")).toBeInTheDocument();
    expect(document.querySelector("details")).toBeInTheDocument();

    // The HTML comment (and its contents) is gone, not shown as literal text.
    expect(screen.queryByText(/SECRETNOISE/)).not.toBeInTheDocument();
    expect(screen.queryByText(/<summary>/)).not.toBeInTheDocument();
  });

  it("shows the empty state when there are no comments", async () => {
    vi.mocked(api.listPrComments).mockResolvedValue([]);
    renderWithProviders(<PrActivityPanel id="s1" viewerIsAuthor={false} />);
    expect(await screen.findByText(/no pr activity yet/i)).toBeInTheDocument();
  });

  it("read-only (non-live): shows comments but hides 'move into conversation'", async () => {
    renderWithProviders(
      <PrActivityPanel id="s1" viewerIsAuthor={false} interactive={false} />,
    );
    // The comment body is shown…
    expect(
      await screen.findByText("Please rename this function."),
    ).toBeInTheDocument();
    // …but the live-only affordances are gone.
    expect(
      screen.queryByRole("button", { name: /move into conversation/i }),
    ).not.toBeInTheDocument();
  });

  it("read-only (non-live): renders nothing when there are no comments", async () => {
    vi.mocked(api.listPrComments).mockResolvedValue([]);
    renderWithProviders(
      <PrActivityPanel id="s1" viewerIsAuthor={false} interactive={false} />,
    );
    // No empty-state placeholder card on a stored session — the whole panel
    // (heading included) is hidden, unlike the live view's "no activity yet".
    await waitFor(() => expect(api.listPrComments).toHaveBeenCalled());
    await waitFor(() =>
      expect(
        screen.queryByRole("button", { name: /pr activity/i }),
      ).not.toBeInTheDocument(),
    );
    expect(screen.queryByText(/no pr activity yet/i)).not.toBeInTheDocument();
  });

  it("'Move into conversation' sends the comment body as a signed reply", async () => {
    const user = userEvent.setup();
    renderWithProviders(<PrActivityPanel id="s1" viewerIsAuthor={false} />);

    await user.click(
      await screen.findByRole("button", { name: /move into conversation/i }),
    );

    await waitFor(() => {
      expect(hostControl.sendReply).toHaveBeenCalledWith(
        "s1",
        "Please rename this function.",
      );
    });
  });

  it("hides the Reply to PR box when viewerIsAuthor is false", async () => {
    renderWithProviders(<PrActivityPanel id="s1" viewerIsAuthor={false} />);
    await screen.findByText("reviewer-bob");
    expect(
      screen.queryByRole("textbox", { name: /reply to pr/i }),
    ).not.toBeInTheDocument();
  });

  it("author can reply to the PR with the right owner/repo/number args", async () => {
    const user = userEvent.setup();
    renderWithProviders(<PrActivityPanel id="s1" viewerIsAuthor={true} />);

    const box = await screen.findByRole("textbox", { name: /reply to pr/i });
    await user.type(box, "Done, renamed.");
    await user.click(screen.getByRole("button", { name: /^send$/i }));

    await waitFor(() => {
      expect(api.replyToPr).toHaveBeenCalledWith("s1", {
        owner: "acme",
        repo: "widgets",
        number: 42,
        body: "Done, renamed.",
      });
    });
  });

  it("author can reply in a review comment's thread (in_reply_to set)", async () => {
    const open: PrComment = {
      ...comment,
      comment_id: "review_comment:987654",
      kind: "review_comment",
      body: "This assertion is off by one.",
      path: "src/main.rs",
      line: 41,
      resolved: false,
    };
    vi.mocked(api.listPrComments).mockResolvedValue([open]);
    const user = userEvent.setup();
    renderWithProviders(<PrActivityPanel id="s1" viewerIsAuthor={true} />);

    // The card offers a "Reply" button; clicking it reveals the composer.
    await user.click(await screen.findByRole("button", { name: /^reply$/i }));
    const box = await screen.findByRole("textbox", {
      name: /reply to review comment/i,
    });
    await user.type(box, "Good catch — fixed.");
    // Scope to the composer's own form — the panel-level "Reply to PR" box has
    // its own Send button too.
    const form = box.closest("form")!;
    await user.click(within(form).getByRole("button", { name: /^send$/i }));

    await waitFor(() => {
      expect(api.replyToPr).toHaveBeenCalledWith("s1", {
        owner: "acme",
        repo: "widgets",
        number: 42,
        body: "Good catch — fixed.",
        in_reply_to: 987654,
      });
    });
  });

  it("hides the threaded Reply button for a non-review-comment id", async () => {
    // A review *summary* (kind "review", id "c1") has no databaseId prefix, so
    // there is no thread to reply into — only "Move into conversation" shows.
    renderWithProviders(<PrActivityPanel id="s1" viewerIsAuthor={true} />);
    await screen.findByText("Please rename this function.");
    expect(
      screen.queryByRole("button", { name: /^reply$/i }),
    ).not.toBeInTheDocument();
  });

  const issueComment: PrComment = {
    ...comment,
    comment_id: "issue:555",
    kind: "issue",
    body: "Can you also update the changelog?",
  };

  it("issue comment: author replies with a top-level PR comment (no in_reply_to)", async () => {
    vi.mocked(api.listPrComments).mockResolvedValue([issueComment]);
    const user = userEvent.setup();
    renderWithProviders(<PrActivityPanel id="s1" viewerIsAuthor={true} />);

    // The issue card offers an inline Reply; clicking reveals the composer.
    await user.click(await screen.findByRole("button", { name: /^reply$/i }));
    const box = await screen.findByRole("textbox", {
      name: /reply to comment/i,
    });
    await user.type(box, "Sure, done.");
    const form = box.closest("form")!;
    await user.click(within(form).getByRole("button", { name: /^send$/i }));

    await waitFor(() => {
      // A reply to an issue comment is a top-level PR comment: NO in_reply_to.
      expect(api.replyToPr).toHaveBeenCalledWith("s1", {
        owner: "acme",
        repo: "widgets",
        number: 42,
        body: "Sure, done.",
      });
    });
  });

  it("issue comment: Reply is greyed out when no CLI is attached", async () => {
    vi.mocked(api.listPrComments).mockResolvedValue([issueComment]);
    renderWithProviders(
      <PrActivityPanel id="s1" viewerIsAuthor={true} cliAttached={false} />,
    );
    expect(
      await screen.findByRole("button", { name: /^reply$/i }),
    ).toBeDisabled();
  });

  it("issue comment: a non-author sees no Reply, only Move into conversation", async () => {
    vi.mocked(api.listPrComments).mockResolvedValue([issueComment]);
    renderWithProviders(<PrActivityPanel id="s1" viewerIsAuthor={false} />);
    await screen.findByText("Can you also update the changelog?");
    expect(
      screen.queryByRole("button", { name: /^reply$/i }),
    ).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /move into conversation/i }),
    ).toBeInTheDocument();
  });

  it("surfaces the not-delivered hint when the PR reply is interrupted", async () => {
    vi.mocked(api.replyToPr).mockResolvedValue({ delivered: false });
    const user = userEvent.setup();
    renderWithProviders(<PrActivityPanel id="s1" viewerIsAuthor={true} />);

    const box = await screen.findByRole("textbox", { name: /reply to pr/i });
    await user.type(box, "ping");
    await user.click(screen.getByRole("button", { name: /^send$/i }));

    expect(
      await screen.findByText(/interrupted — no live cli attached/i),
    ).toBeInTheDocument();
  });

  it("syncs PR messages shortly after a delivered reply", async () => {
    const user = userEvent.setup();
    renderWithProviders(<PrActivityPanel id="s1" viewerIsAuthor={true} />);

    const box = await screen.findByRole("textbox", { name: /reply to pr/i });
    await user.type(box, "Done, renamed.");
    await user.click(screen.getByRole("button", { name: /^send$/i }));

    await waitFor(() => expect(api.replyToPr).toHaveBeenCalled());
    // The sync is deferred (~2s), never fired synchronously with the reply.
    expect(api.syncPrs).not.toHaveBeenCalled();
    await waitFor(() => expect(api.syncPrs).toHaveBeenCalledWith("s1"), {
      timeout: 3000,
    });
  });

  it("does NOT sync when the reply is interrupted (no CLI attached)", async () => {
    vi.mocked(api.replyToPr).mockResolvedValue({ delivered: false });
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      const user = userEvent.setup({
        advanceTimers: vi.advanceTimersByTime,
      });
      renderWithProviders(<PrActivityPanel id="s1" viewerIsAuthor={true} />);

      const box = await screen.findByRole("textbox", {
        name: /reply to pr/i,
      });
      await user.type(box, "ping");
      await user.click(screen.getByRole("button", { name: /^send$/i }));

      await screen.findByText(/interrupted — no live cli attached/i);
      // Outlast the ~2s window a delivered reply's deferred sync fires in.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(2500);
      });
      expect(api.syncPrs).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });
});
