import "fake-indexeddb/auto";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient } from "@tanstack/react-query";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import type { Summary, Visibility } from "@/lib/types";
import { SessionList } from "./SessionList";
import { PrivateSessionTabs } from "@/components/LiveTabs";
import type { PrivateChannel } from "@/lib/privateChannel";
import { activeHost } from "@/lib/hostPairing";

function summary(
  id: string,
  owner_email: string | null,
  visibility: Visibility,
): Summary {
  return {
    id,
    title: id,
    project_path: null,
    git_branch: null,
    message_count: 1,
    started_at: null,
    ended_at: null,
    uploaded_by: owner_email ?? "anon",
    uploaded_at: "2026-06-06T00:00:00.000Z",
    source_host: null,
    owner_email,
    visibility,
    repos: [],
  };
}

const SESSIONS = [
  summary("mine-1", "me@x.com", "organization"),
  summary("other-org", "bob@x.com", "organization"),
  summary("shared-1", "bob@x.com", "restricted"),
];

vi.mock("@/lib/api", () => ({
  api: {
    listSessions: vi.fn(),
    listProjects: vi.fn().mockResolvedValue([]),
    liveSessions: vi.fn().mockResolvedValue([]),
    getUserSettings: vi.fn().mockResolvedValue({
      container_paths: [],
      default_agent: "claude",
    }),
  },
}));

import { api } from "@/lib/api";
const mockedList = vi.mocked(api.listSessions);

// Signed-in user "me@x.com" so the identity-relative tabs render.
vi.mock("@/lib/useAuth", () => ({
  useAuth: () => ({ user: { email: "me@x.com", display_name: null } }),
  useRedirectOnUnauthorized: () => {},
}));

beforeEach(() => {
  localStorage.clear();
  vi.clearAllMocks();
  mockedList.mockResolvedValue(SESSIONS);
});

describe("SessionList scope tabs", () => {
  it("shows all sessions by default and filters when a tab is chosen", async () => {
    const user = userEvent.setup();
    renderWithProviders(<SessionList />);

    // Default "All" shows every visible session.
    await screen.findByText("mine-1");
    expect(screen.getByText("other-org")).toBeInTheDocument();
    expect(screen.getByText("shared-1")).toBeInTheDocument();

    // My sessions → only the one I own.
    await user.click(screen.getByRole("tab", { name: /my sessions/i }));
    expect(screen.getByText("mine-1")).toBeInTheDocument();
    expect(screen.queryByText("other-org")).not.toBeInTheDocument();
    expect(screen.queryByText("shared-1")).not.toBeInTheDocument();

    // Shared with me → only the restricted session shared by someone else.
    await user.click(screen.getByRole("tab", { name: /shared with me/i }));
    expect(screen.getByText("shared-1")).toBeInTheDocument();
    expect(screen.queryByText("mine-1")).not.toBeInTheDocument();
    expect(screen.queryByText("other-org")).not.toBeInTheDocument();

    // Organization → only org-wide sessions from others.
    await user.click(screen.getByRole("tab", { name: /organization/i }));
    expect(screen.getByText("other-org")).toBeInTheDocument();
    expect(screen.queryByText("mine-1")).not.toBeInTheDocument();
    expect(screen.queryByText("shared-1")).not.toBeInTheDocument();
  });

  it("sorts by last activity by default, and by created date when chosen", async () => {
    // `started_at` is creation order; `ended_at` is last-activity order. They
    // disagree so the two sort fields produce visibly different orders.
    const s = (id: string, started_at: string, ended_at: string): Summary => ({
      ...summary(id, "me@x.com", "organization"),
      started_at,
      ended_at,
    });
    mockedList.mockResolvedValue([
      // created oldest, but most recently active.
      s("a", "2026-01-01T00:00:00.000Z", "2026-06-10T00:00:00.000Z"),
      // created middle, least recently active.
      s("b", "2026-03-01T00:00:00.000Z", "2026-04-01T00:00:00.000Z"),
      // created newest, middling activity.
      s("c", "2026-05-01T00:00:00.000Z", "2026-05-15T00:00:00.000Z"),
    ]);
    const user = userEvent.setup();
    renderWithProviders(<SessionList />);

    await screen.findByText("a");
    const order = () =>
      screen.getAllByRole("heading", { level: 3 }).map((h) => h.textContent);

    // Default: most recently active first (by ended_at).
    expect(order()).toEqual(["a", "c", "b"]);

    // Switch to "Newest created" — orders by started_at descending.
    await user.click(screen.getByRole("button", { name: /sort sessions/i }));
    await user.click(screen.getByRole("menuitem", { name: /newest created/i }));
    expect(order()).toEqual(["c", "b", "a"]);

    // "Oldest created" reverses it.
    await user.click(screen.getByRole("button", { name: /sort sessions/i }));
    await user.click(screen.getByRole("menuitem", { name: /oldest created/i }));
    expect(order()).toEqual(["a", "b", "c"]);
  });

  it("shows a scope-specific empty state when a tab has no sessions", async () => {
    // Only a session I own — so "Shared with me" is empty.
    mockedList.mockResolvedValue([
      summary("mine-only", "me@x.com", "organization"),
    ]);
    const user = userEvent.setup();
    renderWithProviders(<SessionList />);

    await screen.findByText("mine-only");
    await user.click(screen.getByRole("tab", { name: /shared with me/i }));

    expect(screen.getByText(/nothing shared with you/i)).toBeInTheDocument();
    expect(screen.queryByText("mine-only")).not.toBeInTheDocument();
  });
});

vi.mock("@/lib/hostControl", async (original) => {
  const actual = await original<typeof import("@/lib/hostControl")>();
  return {
    ...actual,
    hostOnline: vi.fn().mockResolvedValue(true),
    unlockAgain: vi.fn(),
    hostControl: { ...actual.hostControl, privateRead: vi.fn() },
  };
});

it("shows laptop sessions on the main page without adding them to central history", async () => {
  const { pairHost } = await import("@/lib/hostPairing");
  const { makeTestHost } = await import("@/lib/testHost");
  const { hostControl } = await import("@/lib/hostControl");
  const host = await makeTestHost();
  await pairHost(host.descriptor, "Laptop", "browser-key");
  vi.mocked(hostControl.privateRead).mockResolvedValue([
    {
      id: "local-only",
      title: "Laptop work",
      agent: "codex",
      cwd: "/work",
      messages: 0,
      running: true,
    },
  ]);
  renderWithProviders(<SessionList />);
  expect(
    await screen.findByRole("link", { name: /^Laptop work\b/ }),
  ).toHaveAttribute(
    "href",
    `/private/local-only?fullscreen=1&host=${activeHost()?.id}`,
  );
  expect(await screen.findByText("mine-1")).toBeInTheDocument();
  expect(mockedList).toHaveBeenCalledWith({ q: "", project: "" });
  expect(hostControl.privateRead).toHaveBeenCalledWith(
    host.id,
    expect.any(Function),
    { allowUnlock: false },
  );
});

it("ranks exact titles ahead of newer partial matches in the hosted collection", async () => {
  mockedList.mockResolvedValue([
    {
      ...summary("partial", "me@x.com", "organization"),
      title: "Fix bug later",
      uploaded_at: "2026-11-05T00:00:00Z",
    },
    {
      ...summary("exact", "me@x.com", "organization"),
      title: " FIX BUG ",
      uploaded_at: "2026-10-01T00:00:00Z",
    },
  ]);
  renderWithProviders(<SessionList />);
  await screen.findByText("Fix bug later");
  await userEvent.type(
    screen.getByRole("searchbox", { name: "Search sessions or PRs" }),
    "fix bug",
  );
  await waitFor(() =>
    expect(
      screen
        .getAllByRole("link")
        .map((link) => link.getAttribute("href"))
        .filter((href) => href?.startsWith("/sessions/")),
    ).toEqual(["/sessions/exact", "/sessions/partial"]),
  );
});

it.each(["remote", "private"])(
  "uses one search and ranks the %s exact match above the other source",
  async (exactSource) => {
    const { pairHost } = await import("@/lib/hostPairing");
    const { makeTestHost } = await import("@/lib/testHost");
    const { hostControl } = await import("@/lib/hostControl");
    const host = await makeTestHost();
    await pairHost(host.descriptor, "Laptop", "browser-key");
    vi.mocked(hostControl.privateRead).mockResolvedValue([
      {
        id: "local",
        title: exactSource === "private" ? " FIX BUG " : "Fix bug later",
        agent: "codex",
        cwd: "/work",
        running: true,
        messages: 1,
        last_activity: "2026-11-05T00:00:00Z",
      },
    ]);
    mockedList.mockImplementation(async ({ q } = {}) =>
      q === "unrelated"
        ? []
        : [
            {
              ...summary("remote", "me@x.com", "organization"),
              title: exactSource === "remote" ? " FIX BUG " : "Fix bug later",
            },
          ],
    );
    renderWithProviders(<SessionList />);
    await screen.findByRole("link", { name: /Fix bug later/i });
    expect(screen.getAllByRole("searchbox")).toHaveLength(1);
    await userEvent.type(screen.getByRole("searchbox"), "fix bug");
    // Running private cards carry `?fullscreen=1` so a click opens the live
    // terminal directly instead of flashing the history view first.
    const expected =
      exactSource === "remote"
        ? [
            "/sessions/remote",
            `/private/local?fullscreen=1&host=${activeHost()?.id}`,
          ]
        : [
            `/private/local?fullscreen=1&host=${activeHost()?.id}`,
            "/sessions/remote",
          ];
    await waitFor(() =>
      expect(
        screen.getAllByRole("link").map((link) => link.getAttribute("href")),
      ).toEqual(expected),
    );
    await userEvent.clear(screen.getByRole("searchbox"));
    await userEvent.type(screen.getByRole("searchbox"), "unrelated");
    await waitFor(() => expect(screen.queryAllByRole("link")).toHaveLength(0));
  },
);

it("renders the same collection locally without requesting central history or live lists", async () => {
  const localHost = await import("@/lib/localHost");
  const { hostControl } = await import("@/lib/hostControl");
  const local = vi
    .spyOn(localHost, "useLocalHost")
    .mockReturnValue({ local: true, authenticated: true, error: null });
  vi.mocked(hostControl.privateRead).mockResolvedValue([
    {
      id: "local",
      title: "Local work",
      agent: "codex",
      cwd: "/work",
      running: true,
      messages: 1,
      last_activity: null,
    },
  ]);
  try {
    renderWithProviders(<SessionList />);
    expect(
      await screen.findByRole("link", { name: /Local work/ }),
    ).toHaveAttribute("href", "/private/local?fullscreen=1");
    expect(screen.getAllByRole("searchbox")).toHaveLength(1);
    expect(screen.getByRole("button", { name: "Sort sessions" })).toBeVisible();
    expect(api.listSessions).not.toHaveBeenCalled();
    expect(api.liveSessions).not.toHaveBeenCalled();
    expect(api.listProjects).not.toHaveBeenCalled();
  } finally {
    local.mockRestore();
  }
});

it("renders shared cards even when the paired private host never responds", async () => {
  const { pairHost } = await import("@/lib/hostPairing");
  const { makeTestHost } = await import("@/lib/testHost");
  const { hostControl } = await import("@/lib/hostControl");
  await pairHost((await makeTestHost()).descriptor, "Laptop", "browser-key");
  // Simulate an unreachable/slow private host: the read never settles.
  vi.mocked(hostControl.privateRead).mockImplementation(
    () => new Promise(() => {}),
  );
  renderWithProviders(<SessionList />);
  // Shared sessions must show, independent of the private host status.
  expect(await screen.findByText("mine-1")).toBeInTheDocument();
  expect(screen.getByText("other-org")).toBeInTheDocument();
  expect(screen.getByText("shared-1")).toBeInTheDocument();
  // The top-level "Loading sessions…" spinner must NOT cover the list — it
  // belongs to shared history, which has already loaded.
  expect(screen.queryByText(/^Loading sessions/)).not.toBeInTheDocument();
});

it.each([
  ["refused", "Your host refused access"],
  ["not_paired", "No host is paired"],
  ["transport", "Your host is not reachable privately"],
] as const)(
  "distinguishes a %s host error while keeping shared history visible",
  async (code, title) => {
    const { pairHost } = await import("@/lib/hostPairing");
    const { makeTestHost } = await import("@/lib/testHost");
    const { hostControl, HostControlError } = await import("@/lib/hostControl");
    await pairHost((await makeTestHost()).descriptor, "Coder", "browser-key");
    vi.mocked(hostControl.privateRead).mockRejectedValue(
      new HostControlError("request rejected", code),
    );
    renderWithProviders(<SessionList />);
    expect(await screen.findByText(title)).toBeVisible();
    expect(screen.getByText("mine-1")).toBeVisible();
    if (code === "refused" || code === "not_paired") {
      expect(screen.getByText(/Settings → Host access/)).toHaveTextContent(
        "request rejected",
      );
      expect(
        screen.getByRole("link", { name: "Set up access" }),
      ).toHaveAttribute("href", "/settings");
      expect(screen.queryByRole("button", { name: "Retry" })).toBeNull();
      expect(
        screen.queryByText("Your host is not reachable privately"),
      ).not.toBeInTheDocument();
    } else {
      expect(
        screen.queryByText(/Settings → Host access/),
      ).not.toBeInTheDocument();
      expect(screen.queryByRole("link", { name: "Set up access" })).toBeNull();
      expect(screen.getByRole("button", { name: "Retry" })).toBeVisible();
    }
  },
);

it("coalesces an eligible private copy by UUID without changing remote-only scope authority", async () => {
  const { pairHost } = await import("@/lib/hostPairing");
  const { makeTestHost } = await import("@/lib/testHost");
  const { hostControl } = await import("@/lib/hostControl");
  await pairHost((await makeTestHost()).descriptor, "Laptop", "browser-key");
  vi.mocked(hostControl.privateRead).mockResolvedValue([
    {
      id: "shared-1",
      title: "Laptop copy",
      agent: "codex",
      cwd: "/work",
      running: true,
      messages: 2,
      last_activity: null,
    },
  ]);
  mockedList.mockResolvedValue([
    { ...summary("shared-1", "bob@x.com", "restricted"), title: "Shared copy" },
  ]);
  renderWithProviders(<SessionList />);
  await screen.findByRole("link", { name: /Laptop copy/ });
  await waitFor(() =>
    expect(
      screen.queryByRole("link", { name: /^Shared copy/ }),
    ).not.toBeInTheDocument(),
  );
  expect(
    screen.getByRole("link", { name: "View shared copy" }),
  ).toHaveAttribute("href", "/sessions/shared-1");
  expect(screen.getByRole("link", { name: /Laptop copy/ })).toHaveAttribute(
    "href",
    `/private/shared-1?fullscreen=1&host=${activeHost()?.id}`,
  );
  await userEvent.click(screen.getByRole("tab", { name: /shared with me/i }));
  expect(
    await screen.findByRole("link", { name: /Shared copy/ }),
  ).toHaveAttribute("href", "/sessions/shared-1");
  expect(
    screen.queryByRole("link", { name: /Laptop copy/ }),
  ).not.toBeInTheDocument();
});

it("offers one setup entry even when shared sessions exist", async () => {
  const { activeHost, unpairHost } = await import("@/lib/hostPairing");
  while (activeHost()) unpairHost(activeHost()!.id);
  renderWithProviders(<SessionList />);
  expect(await screen.findByText("mine-1")).toBeVisible();
  expect(
    screen.getByRole("heading", { name: "Set up this browser" }),
  ).toBeVisible();
  expect(
    screen.getByText(/approve this browser’s fingerprint on your machine/),
  ).toBeVisible();
  expect(screen.getByRole("link", { name: "Set up a machine" })).toHaveAttribute(
    "href",
    "/settings",
  );
  expect(
    screen.queryByRole("button", { name: "Connect this device" }),
  ).toBeNull();
  expect(screen.queryByRole("textbox", { name: "Setup link" })).toBeNull();
});

it("shows running private sessions while the archive is still loading", async () => {
  const { pairHost } = await import("@/lib/hostPairing");
  const { makeTestHost } = await import("@/lib/testHost");
  const { hostControl } = await import("@/lib/hostControl");
  await pairHost((await makeTestHost()).descriptor, "Laptop", "browser-key");
  vi.mocked(hostControl.privateRead)
    .mockImplementationOnce(() => new Promise(() => {}))
    .mockResolvedValueOnce([
      {
        id: "running-only",
        title: "Running now",
        agent: "codex",
        cwd: "/fixture",
        running: true,
        messages: 0,
        last_activity: null,
      },
    ]);
  renderWithProviders(<SessionList />);
  expect(
    await screen.findByRole("link", { name: /Running now/ }),
  ).toBeVisible();
});

it("keeps a private conversation in Sessions after ending it from its live tab", async () => {
  const { pairHost } = await import("@/lib/hostPairing");
  const { makeTestHost } = await import("@/lib/testHost");
  const { hostControl } = await import("@/lib/hostControl");
  const host = await makeTestHost();
  await pairHost(host.descriptor, "Laptop", "browser-key");
  const session = {
    id: "empty-conversation",
    title: "Fresh conversation",
    agent: "codex",
    cwd: "/work",
    running: true,
    reopenable: true,
    messages: 0,
    last_activity: null,
  };
  let running = true;
  const channel = {
    sessionsList: async (
      _after: unknown,
      _limit: unknown,
      runningOnly: boolean,
    ) => ({
      status: "executed",
      detail: null,
      body: {
        kind: "sessions",
        sessions: runningOnly && !running ? [] : [{ ...session, running }],
        next: null,
      },
    }),
  } as PrivateChannel;
  vi.mocked(hostControl.privateRead).mockImplementation((_, read) =>
    read(channel),
  );
  const end = vi.spyOn(hostControl, "endSession").mockImplementation(async () => {
    running = false;
  });
  try {
    const { queryClient } = renderWithProviders(
      <>
        <PrivateSessionTabs
          sessions={[session]}
          hostId={host.id}
          currentId={session.id}
        />
        <SessionList />
      </>,
    );
    const name = /Fresh conversation/;
    expect(await screen.findByRole("link", { name })).toHaveAttribute(
      "href",
      `/private/${session.id}?fullscreen=1&host=${host.id}`,
    );
    await userEvent.click(
      screen.getByRole("button", { name: "End Fresh conversation" }),
    );
    expect(end).toHaveBeenCalledWith(session.id, host.id);
    await waitFor(() => {
      expect(queryClient.isFetching({ queryKey: ["private", host.id] })).toBe(0);
      expect(screen.getByRole("link", { name })).toHaveAttribute(
        "href",
        `/private/${session.id}?host=${host.id}`,
      );
    });
  } finally {
    end.mockRestore();
  }
});

it("does not read private content or start a passkey prompt before the user unlocks", async () => {
  const { pairHost } = await import("@/lib/hostPairing");
  const { makeTestHost } = await import("@/lib/testHost");
  const { hostControl, unlockAgain } = await import("@/lib/hostControl");
  const host = await makeTestHost();
  await pairHost(host.descriptor, "Laptop");
  const cache = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  cache.setQueryData(
    ["private", host.id, "sessions"],
    [
      {
        id: "old-private",
        title: "Cached private title",
        agent: "codex",
        cwd: "/private",
        running: true,
        messages: 0,
        last_activity: null,
      },
    ],
  );
  vi.mocked(unlockAgain).mockRejectedValue(
    new Error("Waiting for laptop approval"),
  );
  renderWithProviders(<SessionList />, { queryClient: cache });
  const unlock = await screen.findByRole("button", {
    name: "Unlock with passkey",
  });
  expect(hostControl.privateRead).not.toHaveBeenCalled();
  expect(screen.queryByText("Cached private title")).not.toBeInTheDocument();
  expect(unlockAgain).not.toHaveBeenCalled();
  await userEvent.click(unlock);
  expect(await screen.findByText("Waiting for laptop approval")).toBeVisible();
  expect(hostControl.privateRead).not.toHaveBeenCalled();
});

it("keeps sessions from both hosts visible when changing the launch host, with host badges and host-specific links", async () => {
  const { pairHost } = await import("@/lib/hostPairing");
  const { makeTestHost } = await import("@/lib/testHost");
  const { hostControl } = await import("@/lib/hostControl");
  const laptop = await makeTestHost(),
    coder = await makeTestHost(),
    offline = await makeTestHost();
  await pairHost(laptop.descriptor, "Laptop", "browser-key");
  await pairHost(offline.descriptor, "Offline desktop", "browser-key");
  await pairHost(coder.descriptor, "Coder", "browser-key");
  vi.mocked(hostControl.privateRead).mockImplementation(async (hostId) => {
    if (hostId === offline.id) throw new Error("Offline host");
    return [
      {
        id: "same-id",
        title: hostId === laptop.id ? "Laptop task" : "Coder task",
        agent: "codex",
        cwd: "/work",
        messages: 1,
        running: true,
        last_activity: null,
      },
    ] as never;
  });
  renderWithProviders(<SessionList />);
  const laptopCard = await screen.findByRole("link", { name: /Laptop task/ });
  const coderCard = await screen.findByRole("link", { name: /Coder task/ });
  expect(within(laptopCard).getByLabelText("Host: Laptop")).toBeVisible();
  expect(within(coderCard).getByLabelText("Host: Coder")).toBeVisible();
  expect(laptopCard).toHaveAttribute(
    "href",
    `/private/same-id?fullscreen=1&host=${laptop.id}`,
  );
  expect(coderCard).toHaveAttribute(
    "href",
    `/private/same-id?fullscreen=1&host=${coder.id}`,
  );
  await userEvent.selectOptions(
    screen.getByRole("combobox", { name: "Session host" }),
    laptop.id,
  );
  expect(activeHost()?.id).toBe(laptop.id);
  expect(laptopCard).toBeVisible();
  expect(coderCard).toBeVisible();
});

it("labels shared panels with the recorded source host", async () => {
  mockedList.mockResolvedValue([
    {
      ...summary("Shared work", "me@x.com", "organization"),
      source_host: "Office desktop",
    },
  ]);
  renderWithProviders(<SessionList />);
  const card = await screen.findByRole("link", { name: /Shared work/ });
  expect(within(card).getByLabelText("Host: Office desktop")).toBeVisible();
});
