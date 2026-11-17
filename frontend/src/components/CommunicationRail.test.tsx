import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import type { SessionCommunications } from "@/lib/types";
import { CommunicationRail } from "./CommunicationRail";

vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/api")>();
  return {
    ...actual,
    api: { listSessionCommunications: vi.fn() },
  };
});

import { api, ApiError } from "@/lib/api";

const mockedList = vi.mocked(api.listSessionCommunications);

/** Let the resolved/rejected query flush into a render. */
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

const DATA: SessionCommunications = {
  project_id: "p1",
  owner_email: "alice@example.com",
  sessions: [
    {
      session_id: "bob-live",
      title: "Bob's live task",
      agent: "codex",
      owner_email: "bob@example.com",
      last_seen_at: "2026-10-24T10:00:00Z",
      connected: true,
      state: "live",
      reachable: true,
      readable: true,
      conversation: {
        sent_count: 2,
        received_count: 1,
        last_message_at: "2026-10-24T10:05:00Z",
        last_direction: "sent",
        last_sent_status: "queued",
        unacknowledged_incoming: 1,
        messages: [
          {
            id: "m1",
            title: "Check the retry handling",
            direction: "sent",
            created_at: "2026-10-24T10:00:00Z",
          },
          {
            id: "m2",
            title: "Review completed",
            direction: "received",
            created_at: "2026-10-24T10:05:00Z",
          },
        ],
      },
    },
    {
      session_id: "carol-hidden",
      title: "Carol's private work",
      agent: "claude",
      owner_email: "carol@example.com",
      last_seen_at: "2026-10-24T09:00:00Z",
      connected: false,
      state: "ended",
      reachable: false,
      readable: false,
      conversation: {
        sent_count: 0,
        received_count: 3,
        last_message_at: "2026-10-24T09:30:00Z",
        last_direction: "received",
        last_sent_status: null,
        unacknowledged_incoming: 0,
        messages: [],
      },
    },
    {
      session_id: "dave-other",
      title: "Dave's uploaded run",
      agent: "claude",
      owner_email: "dave@example.com",
      last_seen_at: "2026-10-23T09:00:00Z",
      connected: false,
      state: "idle",
      reachable: true,
      readable: true,
      conversation: null,
    },
  ],
};

describe("CommunicationRail", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
    vi.spyOn(Date, "now").mockReturnValue(Date.parse("2026-10-10T12:00:00Z"));
  });

  afterEach(() => vi.restoreAllMocks());

  it("separates real conversations from sessions it could message", async () => {
    mockedList.mockResolvedValue(DATA);
    renderWithProviders(<CommunicationRail sessionId="alice-session" />);

    const conversations = await screen.findByRole("region", {
      name: "Conversations",
    });
    const eligible = screen.getByRole("region", { name: "Can message" });
    expect(
      within(conversations).getByText("Bob's live task"),
    ).toBeInTheDocument();
    expect(
      within(conversations).getByText("Carol's private work"),
    ).toBeInTheDocument();
    expect(
      within(eligible).getByText("Dave's uploaded run"),
    ).toBeInTheDocument();
    expect(
      within(eligible).queryByText("Bob's live task"),
    ).not.toBeInTheDocument();
  });

  it("lists exchanged message titles as bullets beneath their conversation", async () => {
    mockedList.mockResolvedValue(DATA);
    renderWithProviders(<CommunicationRail sessionId="alice-session" />);
    const row = await screen.findByTestId("comm-row-bob-live");
    const messages = within(row).getByRole("list", {
      name: "Exchanged messages",
    });
    expect(
      within(messages)
        .getAllByRole("listitem")
        .map((item) => item.textContent),
    ).toEqual(["Check the retry handling", "Review completed"]);
    expect(
      within(messages).getByText("Check the retry handling").closest("li"),
    ).toHaveAttribute("title", expect.stringContaining("Sent"));
    expect(
      within(messages).getByText("Review completed").closest("li"),
    ).toHaveAttribute("title", expect.stringContaining("Received"));
    expect(
      within(screen.getByTestId("comm-row-dave-other")).queryByRole("list", {
        name: "Exchanged messages",
      }),
    ).not.toBeInTheDocument();
  });

  it("states message counts and delivery states without overclaiming", async () => {
    mockedList.mockResolvedValue(DATA);
    renderWithProviders(<CommunicationRail sessionId="alice-session" />);

    const bob = await screen.findByTestId("comm-row-bob-live");
    expect(bob).toHaveTextContent("2 sent · 1 received");
    expect(bob).toHaveTextContent(/last sent: queued/i);
    expect(bob).toHaveTextContent("1 not acknowledged");
    const attached = within(bob).getByText("Live");
    expect(attached).toHaveAttribute(
      "title",
      expect.stringMatching(/not proof/i),
    );
    // Nothing in the rail claims presence, activity, or delivery.
    const rail = screen.getByRole("complementary", { name: "Communication" });
    expect(rail).not.toHaveTextContent(/online|delivered|read by/i);

    const carol = screen.getByTestId("comm-row-carol-hidden");
    expect(carol).toHaveTextContent("0 sent · 3 received");
    expect(carol).not.toHaveTextContent(/last sent/i);
    expect(within(carol).queryByText("Live")).not.toBeInTheDocument();
  });

  it("links a transcript only when the viewer can read it", async () => {
    mockedList.mockResolvedValue(DATA);
    renderWithProviders(<CommunicationRail sessionId="alice-session" />);

    expect(
      await screen.findByRole("link", { name: "Bob's live task" }),
    ).toHaveAttribute("href", "/sessions/bob-live");
    expect(
      screen.getByRole("link", { name: "Dave's uploaded run" }),
    ).toHaveAttribute("href", "/sessions/dave-other");
    expect(
      screen.queryByRole("link", { name: "Carol's private work" }),
    ).not.toBeInTheDocument();
    expect(screen.getByTestId("comm-row-carol-hidden")).toHaveTextContent(
      "No transcript access",
    );
  });

  it("names other owners", async () => {
    mockedList.mockResolvedValue(DATA);
    renderWithProviders(<CommunicationRail sessionId="alice-session" />);

    expect(await screen.findByTestId("comm-row-bob-live")).toHaveTextContent(
      "bob@example.com",
    );
    expect(screen.getByTestId("comm-row-dave-other")).toHaveTextContent(
      "dave@example.com",
    );
  });

  it("keeps ownerless peers visible when authentication is off", async () => {
    mockedList.mockResolvedValue({
      ...DATA,
      owner_email: null,
      sessions: [{ ...DATA.sessions[2], owner_email: null }],
    });
    renderWithProviders(<CommunicationRail sessionId="alice-session" />);
    expect(
      await screen.findByTestId("comm-row-dave-other"),
    ).toBeInTheDocument();
  });

  it("renders nothing for a viewer who does not own the session", async () => {
    mockedList.mockRejectedValue(new ApiError(403, "only the session owner"));
    renderWithProviders(<CommunicationRail sessionId="alice-session" />);
    await waitFor(() => expect(mockedList).toHaveBeenCalled());
    await settle();
    expect(
      screen.queryByRole("complementary", { name: "Communication" }),
    ).not.toBeInTheDocument();
  });

  it("renders nothing when the session has no communication peers", async () => {
    mockedList.mockResolvedValue({ ...DATA, sessions: [] });
    renderWithProviders(<CommunicationRail sessionId="alice-session" />);
    await waitFor(() => expect(mockedList).toHaveBeenCalled());
    await settle();
    expect(
      screen.queryByRole("complementary", { name: "Communication" }),
    ).not.toBeInTheDocument();
  });

  it("starts collapsed on a phone-width viewport unless the viewer chose otherwise", async () => {
    mockedList.mockResolvedValue(DATA);
    const original = window.matchMedia;
    window.matchMedia = vi.fn().mockImplementation((query: string) => ({
      matches: query === "(max-width: 767px)",
      media: query,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    }));
    try {
      const first = renderWithProviders(
        <CommunicationRail sessionId="alice-session" />,
      );
      expect(
        await screen.findByRole("button", { name: "Expand communication (3)" }),
      ).toBeInTheDocument();
      first.unmount();

      localStorage.setItem("conversations.communicationRailCollapsed", "false");
      renderWithProviders(<CommunicationRail sessionId="alice-session" />);
      expect(
        await screen.findByRole("button", { name: "Collapse communication" }),
      ).toBeInTheDocument();
    } finally {
      window.matchMedia = original;
    }
  });

  it("collapses to a count and remembers the choice", async () => {
    mockedList.mockResolvedValue(DATA);
    const user = userEvent.setup();
    const first = renderWithProviders(
      <CommunicationRail sessionId="alice-session" />,
    );
    await user.click(
      await screen.findByRole("button", { name: "Collapse communication" }),
    );
    expect(
      screen.getByRole("button", { name: "Expand communication (3)" }),
    ).toBeInTheDocument();
    expect(screen.queryByText("Bob's live task")).not.toBeInTheDocument();
    first.unmount();

    renderWithProviders(<CommunicationRail sessionId="other-session" />);
    expect(
      await screen.findByRole("button", { name: "Expand communication (3)" }),
    ).toBeInTheDocument();
  });

  describe("reachability", () => {
    const peer = (
      id: string,
      state: "live" | "idle" | "ended",
      owner: string,
      conversation = false,
    ): SessionCommunications["sessions"][number] => ({
      session_id: id,
      title: `title ${id}`,
      agent: "claude",
      owner_email: owner,
      last_seen_at: "2026-10-10T09:00:00Z",
      connected: state === "live",
      state,
      reachable: state !== "ended" || owner === "alice@example.com",
      readable: true,
      conversation: conversation
        ? {
            sent_count: 1,
            received_count: 0,
            last_message_at: "2026-10-10T09:00:00Z",
            last_direction: "sent",
            last_sent_status: "queued",
            unacknowledged_incoming: 0,
            messages: [],
          }
        : null,
    });
    const POOL = {
      project_id: "p1",
      owner_email: "alice@example.com",
      sessions: [
        peer("live", "live", "bob@example.com"),
        peer("idle", "idle", "bob@example.com"),
        peer("own-ended", "ended", "alice@example.com"),
        peer("other-ended", "ended", "bob@example.com"),
        peer("talked-ended", "ended", "bob@example.com", true),
      ],
    };

    it.each([
      ["live", "can message", "Live"],
      ["idle", "can message", "Idle"],
      ["talked-ended", "conversations", "Ended"],
    ])(
      "lists %s under %s with its state in words",
      async (id, region, word) => {
        mockedList.mockResolvedValue(POOL);
        renderWithProviders(<CommunicationRail sessionId="alice-session" />);
        const row = await screen.findByTestId(`comm-row-${id}`);
        expect(
          within(
            screen.getByRole("region", { name: new RegExp(region, "i") }),
          ).getByTestId(`comm-row-${id}`),
        ).toBe(row);
        expect(within(row).getByText(word)).toBeInTheDocument();
      },
    );

    it("hides own sessions without messages but keeps own conversations", async () => {
      mockedList.mockResolvedValue({
        ...POOL,
        sessions: [
          peer("own-live", "live", "alice@example.com"),
          peer("own-ended", "ended", "alice@example.com"),
          peer("own-talked", "ended", "alice@example.com", true),
          peer("other-live", "live", "bob@example.com"),
        ],
      });
      const user = userEvent.setup();
      renderWithProviders(<CommunicationRail sessionId="alice-session" />);
      await screen.findByTestId("comm-row-own-talked");
      expect(screen.queryByTestId("comm-row-own-live")).not.toBeInTheDocument();
      expect(
        screen.queryByTestId("comm-row-own-ended"),
      ).not.toBeInTheDocument();
      expect(
        within(screen.getByRole("region", { name: "Can message" })).getByTestId(
          "comm-row-other-live",
        ),
      ).toBeInTheDocument();
      await user.click(
        screen.getByRole("button", { name: "Collapse communication" }),
      );
      expect(
        screen.getByRole("button", { name: "Expand communication (2)" }),
      ).toBeInTheDocument();
    });

    it("renders nothing when only own sessions without messages remain", async () => {
      mockedList.mockResolvedValue({
        ...POOL,
        sessions: [peer("own", "live", "alice@example.com")],
      });
      renderWithProviders(<CommunicationRail sessionId="alice-session" />);
      await waitFor(() => expect(mockedList).toHaveBeenCalled());
      await settle();
      expect(
        screen.queryByRole("complementary", { name: "Communication" }),
      ).not.toBeInTheDocument();
    });

    it("hides another person's ended session until Show all", async () => {
      mockedList.mockResolvedValue(POOL);
      const user = userEvent.setup();
      renderWithProviders(<CommunicationRail sessionId="alice-session" />);
      await screen.findByTestId("comm-row-live");
      expect(
        screen.queryByTestId("comm-row-other-ended"),
      ).not.toBeInTheDocument();

      await user.click(screen.getByRole("button", { name: /show all \(1\)/i }));
      const hidden = screen.getByRole("region", { name: "Not reachable" });
      expect(
        within(hidden).getByTestId("comm-row-other-ended"),
      ).toHaveTextContent("Ended");

      await user.click(screen.getByRole("button", { name: /hide ended/i }));
      expect(
        screen.queryByTestId("comm-row-other-ended"),
      ).not.toBeInTheDocument();
    });

    it("expires unreachable peers after 24 hours even when Show all is open", async () => {
      const ended = (id: string, ageMs: number) => ({
        ...peer(id, "ended", "bob@example.com"),
        last_seen_at: new Date(Date.now() - ageMs).toISOString(),
      });
      const day = 24 * 60 * 60 * 1000;
      mockedList.mockResolvedValue({
        ...POOL,
        sessions: [
          peer("live", "live", "bob@example.com"),
          ended("boundary", day),
          ended("stale", day + 1),
        ],
      });
      const user = userEvent.setup();
      renderWithProviders(<CommunicationRail sessionId="alice-session" />);
      await user.click(
        await screen.findByRole("button", { name: "Show all (1)" }),
      );
      expect(screen.getByTestId("comm-row-boundary")).toBeInTheDocument();
      expect(screen.queryByTestId("comm-row-stale")).not.toBeInTheDocument();
    });

    it("renders no rail for only stale unreachable peers", async () => {
      mockedList.mockResolvedValue({
        ...POOL,
        sessions: [
          {
            ...peer("stale", "ended", "bob@example.com"),
            last_seen_at: "2026-10-08T12:00:00Z",
          },
        ],
      });
      renderWithProviders(<CommunicationRail sessionId="alice-session" />);
      await waitFor(() => expect(mockedList).toHaveBeenCalled());
      await settle();
      expect(
        screen.queryByRole("complementary", { name: "Communication" }),
      ).not.toBeInTheDocument();
    });

    it("offers no toggle when nothing is hidden", async () => {
      mockedList.mockResolvedValue({
        ...POOL,
        sessions: POOL.sessions.slice(0, 3),
      });
      renderWithProviders(<CommunicationRail sessionId="alice-session" />);
      await screen.findByTestId("comm-row-live");
      expect(
        screen.queryByRole("button", { name: /show all/i }),
      ).not.toBeInTheDocument();
    });
  });
});

vi.mock("@/lib/privateCommunications", async (importOriginal) => {
  const actual =
    await importOriginal<typeof import("@/lib/privateCommunications")>();
  return { ...actual, readPrivateCommunications: vi.fn() };
});
it("uses shared rail presentation for private exchanges without inventing presence or cloud links", async () => {
  const { readPrivateCommunications } =
    await import("@/lib/privateCommunications");
  vi.mocked(readPrivateCommunications).mockResolvedValue({
    sessions: [
      {
        session_id: "foreign",
        host_id: "pin",
        title: "Pinned peer",
        agent: null,
        last_seen_at: null,
        connected: null,
        state: "unknown",
        reachable: null,
        readable: false,
        conversation: {
          sent_count: 1,
          received_count: 0,
          last_message_at: null,
          last_direction: null,
          last_sent_status: "uncertain",
          unacknowledged_incoming: 0,
          messages: [
            {
              id: "sent:one",
              title: "Please review",
              direction: "sent",
              created_at: null,
              status: "uncertain",
            },
          ],
        },
      },
    ],
  });
  vi.mocked(api.listSessionCommunications).mockClear();
  renderWithProviders(<CommunicationRail sessionId="private" hostId="host" />);
  expect(await screen.findByText("Pinned peer")).toBeVisible();
  expect(screen.getByText("Unknown")).toBeVisible();
  expect(screen.getByText("last sent: uncertain")).toBeVisible();
  expect(screen.queryByRole("link")).toBeNull();
  expect(screen.queryByText("Can message")).toBeNull();
  expect(api.listSessionCommunications).not.toHaveBeenCalled();
});

it("keeps a failed private communication rail collapsed until expanded", async () => {
  const { readPrivateCommunications } =
    await import("@/lib/privateCommunications");
  vi.mocked(readPrivateCommunications).mockRejectedValueOnce(
    new Error("Host offline"),
  );
  localStorage.setItem("conversations.communicationRailCollapsed", "true");
  try {
    renderWithProviders(
      <CommunicationRail sessionId="private-error" hostId="host" />,
    );
    const expand = await screen.findByRole("button", {
      name: "Expand communication",
    });
    expect(expand).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    await userEvent.click(expand);
    expect(await screen.findByRole("alert")).toHaveTextContent("Host offline");
    expect(screen.getByRole("button", { name: "Retry" })).toBeVisible();
  } finally {
    localStorage.removeItem("conversations.communicationRailCollapsed");
  }
});
