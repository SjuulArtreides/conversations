import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Route, Routes } from "react-router-dom";
import { renderWithProviders } from "@/test/utils";
import { SessionDetail } from "./SessionDetail";

// Mock the API module.
vi.mock("@/lib/api", async () => {
  const actual = await vi.importActual<typeof import("@/lib/api")>("@/lib/api");
  return {
    ...actual,
    api: {
      authConfig: vi.fn().mockResolvedValue(null),
      notifications: vi.fn().mockResolvedValue([]),
      projects: vi.fn().mockResolvedValue([]),
      getSession: vi.fn(),
      listSessionPrs: vi.fn().mockResolvedValue([]),
      listSessionLinearDocs: vi.fn().mockResolvedValue([]),
      unlinkSessionPr: vi.fn().mockResolvedValue(undefined),
      syncPrs: vi.fn().mockResolvedValue({ delivered: true }),
      sessionChats: vi.fn().mockResolvedValue([]),
      llmModels: vi.fn().mockResolvedValue([]),
      llmStatus: vi.fn().mockResolvedValue({ enabled: false, model: null }),
      // StartNewSessionButton (in the header) reads the default agent.
      getUserSettings: vi.fn().mockResolvedValue({
        container_paths: [],
        default_agent: "claude",
      }),
      // The on-top LiveTabs polls the active live set; empty ⇒ no bar rendered.
      liveSessions: vi.fn().mockResolvedValue([]),
    },
  };
});

// Keep the embedded ChatPanel and useLlmStatus inert.
const control = vi.hoisted(() => ({ available: false }));
vi.mock("@/lib/useControlAvailable", () => ({
  useControlAvailable: () => ({
    available: control.available,
    isLoading: false,
  }),
}));
vi.mock("@/lib/useLlmStatus", () => ({
  useLlmStatus: () => ({ enabled: false, model: null, isLoading: false }),
}));

import { api } from "@/lib/api";

const baseSession = {
  id: "s1",
  title: "My session",
  project_path: null,
  git_branch: null,
  message_count: 5,
  started_at: null,
  ended_at: null,
  uploaded_by: "test@example.com",
  uploaded_at: "2026-06-06T00:00:00Z",
  source_host: null,
  owner_email: null,
  visibility: "organization" as const,
  repos: [],
  cwd: null,
  claude_version: null,
  skipped_lines: 0,
  shared_emails: [],
  messages: [],
  summary: null,
  project_id: null,
};

function renderDetail() {
  return renderWithProviders(
    <Routes>
      <Route path="/sessions/:id" element={<SessionDetail />} />
    </Routes>,
    { route: "/sessions/s1" },
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  control.available = false;
  vi.mocked(api.authConfig).mockResolvedValue(null);
  vi.mocked(api.getSession).mockResolvedValue(baseSession);
  vi.mocked(api.listSessionPrs).mockResolvedValue([]);
  vi.mocked(api.unlinkSessionPr).mockResolvedValue(undefined);
  vi.mocked(api.sessionChats).mockResolvedValue([]);
  vi.mocked(api.llmModels).mockResolvedValue([]);
  vi.mocked(api.syncPrs).mockResolvedValue({ delivered: true });
});

describe("SessionDetail owner-only controls", () => {
  const controls = [
    ["Share", "Share session"],
    ["Edit", "Edit repositories"],
    ["Rename", "Rename session"],
    ["Delete", "Delete session"],
  ] as const;

  describe.each([
    ["owner", "owner@example.com", "owner@example.com", true],
    ["other viewer", "viewer@example.com", "owner@example.com", false],
    ["ownerless", "viewer@example.com", null, false],
    ["auth disabled", null, null, true],
  ] as const)("%s", (_label, viewer, owner, allowed) => {
    it.each(controls)(
      "gates %s without offering a rejected edit",
      async (name, dialog) => {
        vi.mocked(api.authConfig).mockResolvedValue(
          viewer
            ? {
                google_enabled: false,
                password_enabled: true,
                user: { email: viewer, display_name: null },
              }
            : null,
        );
        vi.mocked(api.getSession).mockResolvedValue({
          ...baseSession,
          owner_email: owner,
        });
        renderDetail();
        await screen.findByRole("heading", { name: "My session" });
        const button = screen.queryByRole("button", { name });
        if (allowed) {
          expect(button).toBeEnabled();
          await userEvent.click(button!);
          expect(
            await screen.findByRole("dialog", { name: dialog }),
          ).toBeInTheDocument();
        } else if (owner === null && name !== "Edit") {
          expect(button).toBeDisabled();
          expect(button).toHaveAccessibleDescription(
            "This session has no owner, so nobody can change it.",
          );
          await userEvent.click(button!);
          expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
        } else {
          expect(button).not.toBeInTheDocument();
          if (owner !== null) {
            expect(
              screen.queryByText(
                /Only the session owner|This session has no owner/,
              ),
            ).not.toBeInTheDocument();
          }
        }
      },
    );
  });

  it("shows no management controls or ownership explanation until auth resolves", async () => {
    let resolveAuth!: (
      value: Awaited<ReturnType<typeof api.authConfig>>,
    ) => void;
    vi.mocked(api.authConfig).mockReturnValue(
      new Promise((resolve) => {
        resolveAuth = resolve;
      }),
    );
    vi.mocked(api.getSession).mockResolvedValue({
      ...baseSession,
      owner_email: "owner@example.com",
    });
    renderDetail();
    await screen.findByRole("heading", { name: "My session" });
    for (const [name] of controls) {
      expect(screen.queryByRole("button", { name })).not.toBeInTheDocument();
    }
    expect(
      screen.queryByText(/Only the session owner|This session has no owner/),
    ).not.toBeInTheDocument();
    resolveAuth({
      google_enabled: false,
      password_enabled: true,
      user: { email: "owner@example.com", display_name: null },
    });
    for (const [name] of controls) {
      expect(await screen.findByRole("button", { name })).toBeEnabled();
    }
    expect(
      screen.queryByText(/Only the session owner|This session has no owner/),
    ).not.toBeInTheDocument();
  });
});

describe("SessionDetail — Pull Requests card", () => {
  it("renders the Pull requests card with empty state", async () => {
    renderDetail();
    expect(await screen.findByText("Pull requests")).toBeInTheDocument();
    expect(
      await screen.findByText(/No pull requests linked/),
    ).toBeInTheDocument();
  });

  it("renders linked PRs from GET /api/sessions/{id}/prs", async () => {
    vi.mocked(api.listSessionPrs).mockResolvedValue([
      {
        url: "https://github.com/acme/repo/pull/7",
        owner: "acme",
        repo: "repo",
        number: 7,
        status: {
          state: "OPEN",
          merged: false,
          draft: false,
          title: "Fix the thing",
        },
      },
    ]);

    renderDetail();

    // The PR row appears with owner/repo#number format.
    expect(await screen.findByText("acme/repo#7")).toBeInTheDocument();
    // The PR title is shown.
    expect(await screen.findByText("Fix the thing")).toBeInTheDocument();
    // The Unlink button is present.
    expect(screen.getByRole("button", { name: "Unlink" })).toBeInTheDocument();
  });

  it("labels merged, draft, and closed PRs in the same session", async () => {
    vi.mocked(api.listSessionPrs).mockResolvedValue([
      {
        url: "https://github.com/acme/repo/pull/2",
        owner: "acme",
        repo: "repo",
        number: 2,
        status: {
          state: "CLOSED",
          merged: true,
          draft: false,
          title: "Merged PR",
        },
      },
      {
        url: "https://github.com/acme/repo/pull/3",
        owner: "acme",
        repo: "repo",
        number: 3,
        status: {
          state: "OPEN",
          merged: false,
          draft: true,
          title: "Draft PR",
        },
      },
      {
        url: "https://github.com/acme/repo/pull/4",
        owner: "acme",
        repo: "repo",
        number: 4,
        status: {
          state: "CLOSED",
          merged: false,
          draft: false,
          title: "Closed PR",
        },
      },
    ]);

    renderDetail();

    const badges = await screen.findAllByTestId("pr-status-badge");
    expect(badges.map((badge) => badge.textContent)).toEqual([
      "Merged",
      "Draft",
      "Closed",
    ]);
  });

  it("shows no status badge but a not-synced hint when status is null", async () => {
    vi.mocked(api.listSessionPrs).mockResolvedValue([
      {
        url: "https://github.com/acme/repo/pull/5",
        owner: "acme",
        repo: "repo",
        number: 5,
        status: null,
        status_fetched_at: null,
      },
    ]);

    renderDetail();
    await screen.findByText("acme/repo#5");
    expect(screen.queryByTestId("pr-status-badge")).not.toBeInTheDocument();
    // The "never synced" hint with the CLI command is shown instead.
    const hint = screen.getByTestId("pr-status-not-synced");
    expect(hint).toHaveTextContent(/status not synced/);
    expect(hint).toHaveTextContent(/needs a connected conversations host\)/);
    // No "as of" timestamp when there's no status.
    expect(screen.queryByTestId("pr-status-synced")).not.toBeInTheDocument();
  });

  it("hides 'Sync now' with no PRs and shows it once PRs are linked", async () => {
    // Empty (the beforeEach default) → no button.
    renderDetail();
    await screen.findByText(/No pull requests linked/);
    expect(screen.queryByTestId("pr-sync-now")).not.toBeInTheDocument();
  });

  it("'Sync now' relays a sync and toasts when a CLI is attached", async () => {
    vi.mocked(api.listSessionPrs).mockResolvedValue([
      {
        url: "https://github.com/acme/repo/pull/5",
        owner: "acme",
        repo: "repo",
        number: 5,
        status: null,
        status_fetched_at: null,
      },
    ]);
    vi.mocked(api.syncPrs).mockResolvedValue({ delivered: true });

    renderDetail();
    const button = await screen.findByTestId("pr-sync-now");
    await userEvent.click(button);

    expect(api.syncPrs).toHaveBeenCalledWith("s1");
    expect(await screen.findByText(/Syncing PRs/)).toBeInTheDocument();
  });

  it("'Sync now' warns when no CLI agent is attached", async () => {
    vi.mocked(api.listSessionPrs).mockResolvedValue([
      {
        url: "https://github.com/acme/repo/pull/5",
        owner: "acme",
        repo: "repo",
        number: 5,
        status: null,
        status_fetched_at: null,
      },
    ]);
    vi.mocked(api.syncPrs).mockResolvedValue({ delivered: false });

    renderDetail();
    const button = await screen.findByTestId("pr-sync-now");
    await userEvent.click(button);

    expect(api.syncPrs).toHaveBeenCalledWith("s1");
    expect(
      await screen.findByText(/No sync agent connected/),
    ).toBeInTheDocument();
  });

  it("shows a 'synced … ago' timestamp when status_fetched_at is present", async () => {
    const fetchedAt = new Date(Date.now() - 3 * 60_000).toISOString(); // 3m ago
    vi.mocked(api.listSessionPrs).mockResolvedValue([
      {
        url: "https://github.com/acme/repo/pull/6",
        owner: "acme",
        repo: "repo",
        number: 6,
        status: {
          state: "OPEN",
          merged: false,
          draft: false,
          title: "Synced PR",
        },
        status_fetched_at: fetchedAt,
      },
    ]);

    renderDetail();

    const synced = await screen.findByTestId("pr-status-synced");
    expect(synced).toHaveTextContent("synced 3m ago");
    // The not-synced hint should NOT appear when status is present.
    expect(
      screen.queryByTestId("pr-status-not-synced"),
    ).not.toBeInTheDocument();
  });

  it("omits the synced timestamp when status_fetched_at is missing", async () => {
    vi.mocked(api.listSessionPrs).mockResolvedValue([
      {
        url: "https://github.com/acme/repo/pull/8",
        owner: "acme",
        repo: "repo",
        number: 8,
        status: {
          state: "OPEN",
          merged: false,
          draft: false,
          title: "No timestamp",
        },
        // status_fetched_at intentionally omitted (older server response).
      },
    ]);

    renderDetail();

    // Badge renders; timestamp is gracefully omitted (no crash).
    await waitFor(() =>
      expect(screen.getByTestId("pr-status-badge")).toHaveTextContent("Open"),
    );
    expect(screen.queryByTestId("pr-status-synced")).not.toBeInTheDocument();
    expect(
      screen.queryByTestId("pr-status-not-synced"),
    ).not.toBeInTheDocument();
  });

  it("shows an 'auto-detected' tag when detected is true", async () => {
    vi.mocked(api.listSessionPrs).mockResolvedValue([
      {
        url: "https://github.com/acme/repo/pull/9",
        owner: "acme",
        repo: "repo",
        number: 9,
        status: {
          state: "OPEN",
          merged: false,
          draft: false,
          title: "Auto PR",
        },
        detected: true,
      },
    ]);

    renderDetail();

    const tag = await screen.findByTestId("pr-auto-detected");
    expect(tag).toHaveTextContent("auto-detected");
  });

  it("omits the 'auto-detected' tag when detected is false or absent", async () => {
    vi.mocked(api.listSessionPrs).mockResolvedValue([
      {
        url: "https://github.com/acme/repo/pull/10",
        owner: "acme",
        repo: "repo",
        number: 10,
        status: {
          state: "OPEN",
          merged: false,
          draft: false,
          title: "Manual PR",
        },
        detected: false,
      },
      {
        url: "https://github.com/acme/repo/pull/11",
        owner: "acme",
        repo: "repo",
        number: 11,
        status: {
          state: "OPEN",
          merged: false,
          draft: false,
          title: "Legacy PR",
        },
        // detected intentionally omitted (older server response).
      },
    ]);

    renderDetail();

    await screen.findByText("acme/repo#10");
    expect(screen.queryByTestId("pr-auto-detected")).not.toBeInTheDocument();
  });

  it("calls unlinkSessionPr when the Unlink button is clicked", async () => {
    vi.mocked(api.listSessionPrs).mockResolvedValue([
      {
        url: "https://github.com/acme/repo/pull/7",
        owner: "acme",
        repo: "repo",
        number: 7,
        status: { state: "OPEN", merged: false, draft: false, title: "Fix" },
      },
    ]);

    const user = userEvent.setup();
    renderDetail();

    await user.click(await screen.findByRole("button", { name: "Unlink" }));

    await waitFor(() =>
      expect(api.unlinkSessionPr).toHaveBeenCalledWith("s1", "acme", "repo", 7),
    );
  });
});

describe("SessionDetail — Repositories container filtering", () => {
  const workRepo = {
    repo: "example-platform",
    remote_url: "https://github.com/acme/example-platform",
    branch: "main",
    pr_url: null,
    paths: [],
  };
  const containerRepo = {
    repo: "example",
    remote_url: "https://github.com/acme/example",
    branch: "main",
    pr_url: null,
    paths: [],
    is_container: true,
  };

  it("hides an is_container repo from the default list and reveals it on toggle", async () => {
    vi.mocked(api.getSession).mockResolvedValue({
      ...baseSession,
      repos: [workRepo, containerRepo],
    });

    const user = userEvent.setup();
    renderDetail();

    // Work repo is shown; container is hidden by default.
    expect(await screen.findByText("example-platform")).toBeInTheDocument();
    expect(screen.queryByText("example")).not.toBeInTheDocument();
    expect(screen.queryByTestId("container-repo-row")).not.toBeInTheDocument();

    // The subtle "hidden" affordance is present; clicking it reveals the row.
    const toggle = screen.getByTestId("container-toggle");
    expect(toggle).toHaveTextContent(/1 container repo hidden/i);
    await user.click(toggle);

    expect(await screen.findByTestId("container-repo-row")).toBeInTheDocument();
    expect(screen.getByText("example")).toBeInTheDocument();
    expect(screen.getByTestId("container-badge")).toHaveTextContent(
      "container",
    );
  });

  it("shows a repo without is_container normally (defensive default)", async () => {
    vi.mocked(api.getSession).mockResolvedValue({
      ...baseSession,
      repos: [workRepo],
    });

    renderDetail();

    expect(await screen.findByText("example-platform")).toBeInTheDocument();
    expect(screen.getByTestId("repo-row")).toBeInTheDocument();
    // No container affordance when there are no container repos.
    expect(screen.queryByTestId("container-toggle")).not.toBeInTheDocument();
  });
});

describe("SessionDetail — Re-open as live", () => {
  it("does not offer reopen with auth disabled even though management is allowed", async () => {
    control.available = true;
    vi.mocked(api.getSession).mockResolvedValue({
      ...baseSession,
      cwd: "/home/dev/repo",
    });
    renderDetail();
    await screen.findByText("My session");
    expect(screen.getByRole("button", { name: "Share" })).toBeEnabled();
    expect(
      screen.queryByRole("button", { name: /re-open as live/i }),
    ).not.toBeInTheDocument();
  });
  it("hides the button when the session has no cwd", async () => {
    vi.mocked(api.getSession).mockResolvedValue({ ...baseSession, cwd: null });
    renderDetail();
    await screen.findByText("My session");
    expect(
      screen.queryByRole("button", { name: /re-open as live/i }),
    ).not.toBeInTheDocument();
  });

  it("shows the button (disabled, no host) when the owner views a session with a cwd", async () => {
    vi.mocked(api.authConfig).mockResolvedValue({
      google_enabled: false,
      password_enabled: true,
      user: { email: "owner@example.com", display_name: null },
    });
    vi.mocked(api.getSession).mockResolvedValue({
      ...baseSession,
      owner_email: "owner@example.com",
      cwd: "/home/dev/repo",
    });
    renderDetail();
    const btn = await screen.findByRole("button", { name: /re-open as live/i });
    expect(btn).toBeDisabled();
  });

  it.each(["other@example.com", null])(
    "does not offer reopen for a session owned by %s to the viewer",
    async (owner_email) => {
      vi.mocked(api.authConfig).mockResolvedValue({
        google_enabled: false,
        password_enabled: true,
        user: { email: "viewer@example.com", display_name: null },
      });
      control.available = true;
      vi.mocked(api.getSession).mockResolvedValue({
        ...baseSession,
        owner_email,
        cwd: "/home/dev/repo",
      });
      renderDetail();
      await screen.findByText("My session");
      expect(
        screen.queryByRole("button", { name: /re-open as live/i }),
      ).not.toBeInTheDocument();
    },
  );
});

describe("SessionDetail — Start new session", () => {
  // Available from a stored session too, so you can spin up a fresh live
  // session without going back to the list. Self-gates on host presence.
  it("shows Start new session, disabled with a tooltip, when no host is attached", async () => {
    control.available = false;
    renderDetail();
    await screen.findByText("My session");
    const btn = await screen.findByRole("button", {
      name: /start new session/i,
    });
    expect(btn).toBeDisabled();
  });

  it("enables Start new session when a CLI host is attached", async () => {
    control.available = true;
    renderDetail();
    // Re-query each poll: the button starts disabled (host poll pending) and
    // HostSplitButton restructures the DOM once a host is found, so a node
    // captured up front would go stale.
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: /start new session/i }),
      ).toBeEnabled(),
    );
  });
});

describe("SessionDetail — handoff startup", () => {
  const failedHandoff = {
    source_session_id: "source-1",
    status: "failed" as const,
    error: "cmux workspace.create: Error: Socket closed before reply",
    created_at: "2026-10-24T14:00:00Z",
  };

  it("shows a failed handoff's error and safe recovery instead of an empty transcript", async () => {
    vi.mocked(api.getSession).mockResolvedValue({
      ...baseSession,
      title: "Handoff: Fix restore",
      message_count: 0,
      handoff: failedHandoff,
    });

    renderDetail();

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent(
      "Host could not confirm this handoff started",
    );
    expect(alert).toHaveTextContent(
      "cmux workspace.create: Error: Socket closed before reply",
    );
    // An ambiguous launch error does not prove that nothing is running.
    expect(alert).toHaveTextContent(/may or may not have started/i);
    expect(alert).toHaveTextContent(/check your terminal app/i);
    expect(alert).not.toHaveTextContent(/close it/i);
    expect(alert).not.toHaveTextContent(/idempotency/i);
    expect(
      screen.getByRole("link", { name: /source session/i }),
    ).toHaveAttribute("href", "/sessions/source-1");
  });

  it("replaces the pending status with the failure without a manual refresh", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      vi.mocked(api.getSession)
        .mockResolvedValueOnce({
          ...baseSession,
          message_count: 0,
          handoff: { ...failedHandoff, status: "starting", error: null },
        })
        .mockResolvedValue({
          ...baseSession,
          message_count: 0,
          handoff: failedHandoff,
        });

      renderDetail();

      expect(await screen.findByRole("status")).toHaveTextContent(
        /waiting for your host to start this handoff/i,
      );
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();

      await vi.advanceTimersByTimeAsync(3000);

      expect(await screen.findByRole("alert")).toHaveTextContent(
        "Socket closed before reply",
      );
      expect(
        screen.queryByText(/waiting for your host/i),
      ).not.toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it.each([
    ["an ordinary session", null],
    [
      "a started handoff",
      { ...failedHandoff, status: "started" as const, error: null },
    ],
  ])("shows no handoff notice for %s", async (_label, handoff) => {
    vi.mocked(api.getSession).mockResolvedValue({ ...baseSession, handoff });

    renderDetail();

    expect(await screen.findByText("My session")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(
      screen.queryByText(/waiting for your host/i),
    ).not.toBeInTheDocument();
  });
});

describe("SessionDetail — write waiting for key trust (EX-03)", () => {
  it("warns a pending writer they can read but not type yet", async () => {
    vi.mocked(api.authConfig).mockResolvedValue({
      google_enabled: false,
      password_enabled: true,
      user: { email: "alice@example.com", display_name: null },
    });
    vi.mocked(api.getSession).mockResolvedValue({
      ...baseSession,
      owner_email: "owner@example.com",
      viewer_write_pending_key: true,
    });
    renderDetail();
    expect(
      await screen.findByText(
        /You can read this, but you can’t type into it yet\./,
      ),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Offer your key" }),
    ).toBeEnabled();
    expect(
      screen.getByText(/Your key isn’t trusted on their host/),
    ).toBeInTheDocument();
  });

  it("shows no write notice for an ordinary reader", async () => {
    vi.mocked(api.authConfig).mockResolvedValue({
      google_enabled: false,
      password_enabled: true,
      user: { email: "alice@example.com", display_name: null },
    });
    vi.mocked(api.getSession).mockResolvedValue({
      ...baseSession,
      owner_email: "owner@example.com",
    });
    renderDetail();
    expect(await screen.findByText("My session")).toBeInTheDocument();
    expect(
      screen.queryByRole("note", { name: /waiting for key trust/i }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Offer your key" }),
    ).not.toBeInTheDocument();
  });

  it("offers a key through the invite link: pasting it routes to the pairing ceremony (EX-03)", async () => {
    vi.mocked(api.authConfig).mockResolvedValue({
      google_enabled: false,
      password_enabled: true,
      user: { email: "alice@example.com", display_name: null },
    });
    vi.mocked(api.getSession).mockResolvedValue({
      ...baseSession,
      owner_email: "owner@example.com",
      viewer_write_pending_key: true,
    });
    const fragment =
      "desc=chd1.demo&challenge=0123456789abcdef0123456789abcdef&options=eyJwdWJsaWNLZXkiOnt9fQ";
    renderWithProviders(
      <Routes>
        <Route path="/sessions/:id" element={<SessionDetail />} />
        <Route
          path="/host-link"
          element={<div data-testid="pairing-ceremony" />}
        />
      </Routes>,
      { route: "/sessions/s1" },
    );
    const user = userEvent.setup();
    await user.click(
      await screen.findByRole("button", { name: "Offer your key" }),
    );
    const input = await screen.findByLabelText("Invite link");
    const submit = screen.getByRole("button", { name: "Offer your key" });
    expect(submit).toBeDisabled();
    await user.type(
      input,
      `https://conversations.example.com/host-link#${fragment}`,
    );
    await user.click(screen.getByRole("button", { name: "Offer your key" }));
    // The fragment carries the whole ceremony and stays in this browser.
    expect(await screen.findByTestId("pairing-ceremony")).toBeInTheDocument();
  });
});
