import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import type { Summary } from "@/lib/types";
import { SessionSearchDialog } from "./SessionSearchDialog";

const navigate = vi.fn();
vi.mock("react-router-dom", async () => {
  const actual =
    await vi.importActual<typeof import("react-router-dom")>(
      "react-router-dom",
    );
  return { ...actual, useNavigate: () => navigate };
});

vi.mock("@/lib/api", () => ({
  api: {
    listSessions: vi.fn(),
  },
}));

import { api } from "@/lib/api";

function summary(overrides: Partial<Summary> = {}): Summary {
  return {
    id: "sess-1",
    title: "Investigate worker incident",
    project_path: "/Users/dev/arrakis-conversations",
    git_branch: "main",
    message_count: 12,
    started_at: "2026-08-27T08:00:00Z",
    ended_at: null,
    uploaded_by: "dev@example.com",
    uploaded_at: "2026-08-27T08:00:00Z",
    source_host: null,
    owner_email: "dev@example.com",
    visibility: "restricted",
    repos: [],
    ...overrides,
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(api.listSessions).mockResolvedValue([summary()]);
});

describe("SessionSearchDialog", () => {
  it("searches with the overview API and opens the selected session", async () => {
    const user = userEvent.setup();
    const onOpenChange = vi.fn();
    renderWithProviders(
      <SessionSearchDialog open onOpenChange={onOpenChange} />,
      { route: "/sessions/current" },
    );

    expect(
      await screen.findByRole("button", {
        name: /open investigate worker incident/i,
      }),
    ).toBeInTheDocument();

    await user.type(screen.getByLabelText(/search sessions or prs/i), "worker");
    await waitFor(() =>
      expect(api.listSessions).toHaveBeenLastCalledWith({ q: "worker" }),
    );
    await user.click(
      await screen.findByRole("button", {
        name: /open investigate worker incident/i,
      }),
    );

    expect(onOpenChange).toHaveBeenCalledWith(false);
    expect(navigate).toHaveBeenCalledWith("/sessions/sess-1");
  });

  it("preserves fullscreen only when opening a live result", async () => {
    vi.mocked(api.listSessions).mockResolvedValue([
      summary({ id: "live-1", title: "Live work", is_live: true }),
    ]);
    const user = userEvent.setup();
    renderWithProviders(<SessionSearchDialog open onOpenChange={() => {}} />, {
      route: "/sessions/current?fullscreen=1",
    });

    await user.click(
      await screen.findByRole("button", { name: /open live work/i }),
    );
    expect(navigate).toHaveBeenCalledWith("/sessions/live-1?fullscreen=1");
  });

  it("moves the highlight with arrow keys and opens the selected result", async () => {
    vi.mocked(api.listSessions).mockResolvedValue([
      summary(),
      summary({ id: "sess-2", title: "Second session" }),
    ]);
    const user = userEvent.setup();
    renderWithProviders(<SessionSearchDialog open onOpenChange={() => {}} />, {
      route: "/sessions/current",
    });

    const searchInput = screen.getByLabelText(/search sessions or prs/i);
    const firstResult = await screen.findByRole("button", {
      name: /open investigate worker incident/i,
    });
    const secondResult = await screen.findByRole("button", {
      name: /open second session/i,
    });

    await user.click(searchInput);
    await user.keyboard("{ArrowUp}{ArrowUp}");
    expect(firstResult).toHaveClass("bg-accent");

    await user.keyboard("{ArrowDown}{ArrowDown}");
    expect(secondResult).toHaveClass("bg-accent");
    expect(screen.getByRole("status")).toHaveTextContent(
      "Selected Second session, 2 of 2.",
    );

    await user.keyboard("{ArrowUp}{ArrowUp}");
    expect(firstResult).toHaveClass("bg-accent");

    await user.keyboard("{ArrowDown}{ArrowDown}");
    expect(secondResult).toHaveClass("bg-accent");

    await user.keyboard("{Enter}");
    expect(navigate).toHaveBeenCalledWith("/sessions/sess-2");
  });

  it("does not activate stale results while a debounced search is pending", async () => {
    let resolveSearch: ((sessions: Summary[]) => void) | undefined;
    vi.mocked(api.listSessions).mockImplementation(({ q }) => {
      if (!q) return Promise.resolve([summary()]);
      return new Promise((resolve) => {
        resolveSearch = resolve;
      });
    });
    const user = userEvent.setup();
    renderWithProviders(<SessionSearchDialog open onOpenChange={() => {}} />, {
      route: "/sessions/current",
    });

    expect(
      await screen.findByRole("button", {
        name: /open investigate worker incident/i,
      }),
    ).toBeInTheDocument();

    const searchInput = screen.getByLabelText(/search sessions or prs/i);
    await user.type(searchInput, "new");
    await user.keyboard("{Enter}");
    expect(navigate).not.toHaveBeenCalled();

    await waitFor(() =>
      expect(api.listSessions).toHaveBeenLastCalledWith({ q: "new" }),
    );
    expect(screen.getByText("Searching sessions…")).toBeInTheDocument();
    await user.keyboard("{Enter}");
    expect(navigate).not.toHaveBeenCalled();

    await act(async () => {
      resolveSearch?.([
        summary({ id: "new-session", title: "New matching session" }),
      ]);
    });
    expect(
      await screen.findByRole("button", {
        name: /open new matching session/i,
      }),
    ).toBeInTheDocument();

    await user.keyboard("{Enter}");
    expect(navigate).toHaveBeenCalledWith("/sessions/new-session");
  });
});

it("searches host metadata through the shared dialog and preserves private fullscreen navigation", async () => {
  const { hostControl } = await import("@/lib/hostControl");
  const read = vi.spyOn(hostControl, "privateRead").mockResolvedValue([
    {
      id: "private-one",
      title: "Local task",
      cwd: "/work",
      agent: "codex",
      running: true,
      messages: 1,
      last_activity: null,
    },
  ]);
  renderWithProviders(
    <SessionSearchDialog open onOpenChange={vi.fn()} hostId="host-a" />,
    { route: "/private/current?fullscreen=1" },
  );
  await userEvent.click(
    await screen.findByRole("button", { name: "Open Local task" }),
  );
  expect(read).toHaveBeenCalledWith("host-a", expect.any(Function));
  expect(api.listSessions).not.toHaveBeenCalled();
  expect(navigate).toHaveBeenLastCalledWith(
    "/private/private-one?fullscreen=1",
  );
  read.mockRestore();
});

it("puts an exact title before partial results in the shared quick switcher", async () => {
  vi.mocked(api.listSessions).mockResolvedValue([
    summary({ id: "partial", title: "Fix bug later" }),
    summary({ id: "exact", title: " FIX BUG " }),
  ]);
  renderWithProviders(<SessionSearchDialog open onOpenChange={vi.fn()} />);
  await screen.findByRole("button", { name: "Open Fix bug later" });
  await userEvent.type(
    screen.getByRole("searchbox", { name: "Search sessions or PRs" }),
    "fix bug",
  );
  await waitFor(() =>
    expect(
      screen.getAllByRole("button", { name: /^Open / })[0],
    ).toHaveAccessibleName("Open FIX BUG"),
  );
});
