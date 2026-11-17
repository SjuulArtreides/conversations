import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { usePairedHosts } from "@/lib/hostPairing";
import { Link, Route, Routes } from "react-router-dom";
import { renderWithProviders } from "@/test/utils";
import type {
  ChannelResult,
  PrivateChannel,
  SessionEntry,
} from "@/lib/privateChannel";

vi.mock("@/components/LinearDocsCard", () => ({ LinearDocsCard: () => null }));

vi.mock("@/lib/privatePrCache", () => ({
  readPrivatePrCache: vi.fn().mockResolvedValue({
    prs: [
      {
        url: "https://github.com/acme/private-shell/pull/23",
        owner: "acme",
        repo: "private-shell",
        number: 23,
        status: null,
      },
    ],
    comments: [],
  }),
}));

vi.mock("@/lib/api", () => ({
  api: { setSharing: vi.fn(), searchUsers: vi.fn().mockResolvedValue([]) },
}));

const { terminalProps } = vi.hoisted(() => ({
  terminalProps: [] as Record<string, unknown>[],
}));
vi.mock("@/components/LiveTerminal", async () => {
  const { PrivateSessionActionsMenu } =
    await import("@/components/PrivateSessionActionsMenu");
  return {
    LiveTerminal: (props: Record<string, unknown>) => {
      terminalProps.push(props);
      const entry = (props.privateSessions as SessionEntry[] | undefined)?.find(
        (session) => session.id === props.id,
      );
      return (
        <div data-testid="live-terminal">
          {entry && (
            <PrivateSessionActionsMenu
              hostId={props.privateHostId as string}
              session={entry}
            />
          )}
        </div>
      );
    },
  };
});

vi.mock("@/lib/hostControl", async () => {
  const actual =
    await vi.importActual<typeof import("@/lib/hostControl")>(
      "@/lib/hostControl",
    );
  return {
    ...actual,
    unlockAgain: vi.fn(),
    hostControl: {
      ...actual.hostControl,
      privateRead: vi.fn(),
      listAccess: vi.fn(),
      sendReply: vi.fn(),
      reopenSession: vi.fn(),
      addGrant: vi.fn(),
      removeGrant: vi.fn(),
      sharePublication: vi.fn(),
      unsharePublication: vi.fn(),
      publicationStatus: vi.fn(),
      passkeyInvite: vi.fn(),
      passkeyPending: vi.fn(),
      passkeyApprove: vi.fn(),
    },
  };
});

import { api } from "@/lib/api";
import {
  hostControl,
  HostControlError,
  unlockAgain,
  type PublicationAudience,
  type PublicationStatus,
} from "@/lib/hostControl";
import { pairHost } from "@/lib/hostPairing";
import { makeTestHost } from "@/lib/testHost";
import { PrivateSessionList, PrivateSessionView } from "./PrivateSessions";

const SESSION = "11111111-2222-3333-4444-555555555555";

function message(uuid: string, role: string, blocks: unknown[]) {
  return {
    uuid,
    parent_uuid: null,
    role,
    kind: "mixed",
    model: null,
    is_sidechain: false,
    timestamp: null,
    blocks,
    text: "",
    raw: {},
    usage: null,
  };
}

/** The host's history as the channel serves it: seq is first appearance. */
let sessionRunning = true;
let reopenable: boolean | undefined;
let resumeMode: "native" | "continuation" | undefined;
let listed: SessionEntry[] | undefined;
let rows: { seq: number; message: Record<string, unknown>; sha256: string }[];
function put(m: Record<string, unknown>) {
  const sha256 = btoa(JSON.stringify(m));
  const row = rows.find((r) => r.message.uuid === m.uuid);
  if (row) Object.assign(row, { message: m, sha256 });
  else rows.push({ seq: rows.length + 1, message: m, sha256 });
}

const ok = (body: ChannelResult["body"]): ChannelResult => ({
  status: "executed",
  detail: null,
  body,
});

const channel = {
  historyGet: async (_s: string, after = 0, limit = 32, digest = false) => {
    const page = rows.filter((r) => r.seq > after).slice(0, limit);
    return ok({
      kind: "history",
      items: page.map((r) => ({
        seq: r.seq,
        uuid: r.message.uuid as string,
        size: 1,
        sha256: r.sha256,
        message: digest ? null : r.message,
      })),
      next: null,
    });
  },
  historySearch: async () =>
    ok({
      kind: "search",
      hits: [{ seq: 1, uuid: "u1", snippet: "cargo test matched" }],
    }),
  sessionsList: async () =>
    ok({
      kind: "sessions",
      sessions: listed ?? [
        {
          id: SESSION,
          agent: "claude",
          cwd: "/work/repo",
          title: "Fix the build",
          prs: ["https://github.com/acme/private-shell/pull/23"],
          messages: 2,
          last_activity: null,
          running: sessionRunning,
          reopenable,
          resume_mode: resumeMode,
        },
      ],
      next: null,
    }),
} as unknown as PrivateChannel;

/** One message, served by a second host's channel under the SAME session id. */
const otherHostChannel = {
  historyGet: async () =>
    ok({
      kind: "history",
      items: [
        {
          seq: 1,
          uuid: "other-1",
          size: 1,
          sha256: "o1",
          message: message("other-1", "user", [
            { type: "text", text: "from the other host" },
          ]),
        },
      ],
      next: null,
    }),
} as unknown as PrivateChannel;

/** What the host holds; share/unshare change it, as the real host does. */
let held: PublicationStatus;
const statusOf = (
  state: PublicationStatus["state"],
  audience: PublicationAudience | null,
): PublicationStatus => ({
  state,
  rev: state === "private" ? 0 : 1,
  audience,
  published: 0,
  pending: 0,
  error: null,
});

let HOST: string;

beforeEach(async () => {
  vi.mocked(hostControl.listAccess).mockRejectedValue(
    new HostControlError(
      "Owner grant required to list approved credentials.",
      "refused",
    ),
  );
  sessionRunning = false;
  reopenable = undefined;
  resumeMode = undefined;
  listed = undefined;
  vi.clearAllMocks();
  terminalProps.length = 0;
  localStorage.clear();
  const host = await makeTestHost();
  await pairHost(host.descriptor, "laptop");
  HOST = host.id;
  rows = [];
  held = statusOf("private", null);
  vi.mocked(hostControl.publicationStatus).mockImplementation(async () => held);
  vi.mocked(hostControl.sharePublication).mockImplementation(
    async (_h, _s, audience) => {
      held = statusOf("syncing", audience);
      return "syncing";
    },
  );
  vi.mocked(hostControl.unsharePublication).mockImplementation(async () => {
    held = statusOf("deletion_pending", null);
    return "deletion_pending";
  });
  vi.mocked(hostControl.privateRead).mockImplementation(((
    hostId: string,
    read: (c: PrivateChannel) => unknown,
  ) =>
    Promise.resolve(
      read(hostId === HOST ? channel : otherHostChannel),
    )) as never);
  vi.mocked(hostControl.sendReply).mockResolvedValue(undefined);
});

afterEach(() => {
  vi.useRealTimers();
});

function SwitchHostLink() {
  const { active } = usePairedHosts();
  return (
    <Link to={`/private/${SESSION}?host=${active}`}>
      Open selected host session
    </Link>
  );
}

const view = () =>
  renderWithProviders(
    <>
      <SwitchHostLink />
      <Routes>
        <Route path="/private/:id" element={<PrivateSessionView />} />
      </Routes>
    </>,
    { route: `/private/${SESSION}` },
  );

describe("private session list", () => {
  it("lists what the host says this browser may read", async () => {
    renderWithProviders(<PrivateSessionList />);
    expect(await screen.findByText("Fix the build")).toBeInTheDocument();
    expect(screen.getByText("repo")).toBeInTheDocument();
  });

  it("keeps running sessions visible when the archive fails", async () => {
    sessionRunning = true;
    const original = channel.sessionsList;
    const spy = vi
      .spyOn(channel, "sessionsList")
      .mockImplementation((after, limit, running) =>
        running
          ? original(after, limit, running)
          : Promise.reject(new Error("archive failed")),
      );
    try {
      renderWithProviders(<PrivateSessionList />);
      expect(
        await screen.findByRole("link", { name: /Fix the build/ }),
      ).toBeVisible();
      expect(screen.queryByText(/archive failed/)).not.toBeInTheDocument();
    } finally {
      spy.mockRestore();
    }
  });

  it("keeps archived sessions visible when the active refresh fails", async () => {
    const original = channel.sessionsList;
    const spy = vi
      .spyOn(channel, "sessionsList")
      .mockImplementation((after, limit, running) =>
        running
          ? Promise.reject(new Error("active refresh failed"))
          : original(after, limit, running),
      );
    try {
      renderWithProviders(<PrivateSessionList />);
      expect(
        await screen.findByRole("link", { name: /Fix the build/ }),
      ).toBeVisible();
      expect(
        screen.queryByText(/active refresh failed/),
      ).not.toBeInTheDocument();
    } finally {
      spy.mockRestore();
    }
  });

  it.each([
    "unknown, revoked or expired credential or delegation",
    "operation outside the delegation's scope",
  ])(
    "explains access refusal without treating it as a network failure: %s",
    async (detail) => {
      vi.mocked(hostControl.privateRead).mockRejectedValue(
        new HostControlError(detail, "refused"),
      );
      renderWithProviders(<PrivateSessionList />);
      expect(await screen.findByText("Your host refused access")).toBeVisible();
      expect(screen.getByText(/Settings → Host access/)).toHaveTextContent(
        detail,
      );
      expect(
        screen.getByRole("link", { name: "Set up access" }),
      ).toHaveAttribute("href", "/settings");
      expect(screen.queryByRole("button", { name: "Retry" })).toBeNull();
      expect(
        screen.queryByText("Your host is not reachable privately"),
      ).not.toBeInTheDocument();
      expect(screen.queryByText("Fix the build")).not.toBeInTheDocument();
      expect(api.setSharing).not.toHaveBeenCalled();
      expect(api.searchUsers).not.toHaveBeenCalled();
    },
  );

  it("says the host is unreachable and shows nothing from anywhere else", async () => {
    vi.mocked(hostControl.privateRead).mockRejectedValue(
      new HostControlError("no route", "transport"),
    );
    renderWithProviders(<PrivateSessionList />);
    expect(
      await screen.findByText("Your host is not reachable privately"),
    ).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: "Set up access" })).toBeNull();
    expect(screen.getByRole("button", { name: "Retry" })).toBeVisible();
  });

  it("links an unpaired browser to access setup without reading private data", async () => {
    localStorage.clear();
    renderWithProviders(<PrivateSessionList />);
    expect(await screen.findByText("No host is paired")).toBeVisible();
    expect(screen.getByRole("link", { name: "Set up access" })).toHaveAttribute(
      "href",
      "/settings",
    );
    expect(hostControl.privateRead).not.toHaveBeenCalled();
    expect(api.setSharing).not.toHaveBeenCalled();
    expect(api.searchUsers).not.toHaveBeenCalled();
  });
});

it("keeps private sessions unavailable before approval and lets an approved device send a reply", async () => {
  let approved = false;
  vi.mocked(hostControl.privateRead).mockImplementation(((
    _hostId: string,
    read: (c: PrivateChannel) => unknown,
  ) =>
    approved
      ? Promise.resolve(read(channel))
      : Promise.reject(
          new HostControlError("Awaiting laptop approval", "refused"),
        )) as never);
  put(
    message("fixture-message", "user", [
      { type: "text", text: "Private fixture content" },
    ]),
  );
  const { queryClient } = renderWithProviders(
    <Routes>
      <Route path="/private" element={<PrivateSessionList />} />
      <Route path="/private/:id" element={<PrivateSessionView />} />
    </Routes>,
    { route: "/private" },
  );
  expect(await screen.findByText(/Awaiting laptop approval/)).toBeVisible();
  expect(screen.queryByText("Fix the build")).not.toBeInTheDocument();
  approved = true;
  await act(async () => {
    await queryClient.invalidateQueries({ queryKey: ["private", HOST] });
  });
  const session = await screen.findByRole("link", { name: /Fix the build/ });
  await userEvent.click(session);
  const reply = await screen.findByLabelText("Reply");
  await userEvent.type(reply, "hello from phone");
  await userEvent.click(screen.getByRole("button", { name: "Send" }));
  await waitFor(() =>
    expect(hostControl.sendReply).toHaveBeenCalledWith(
      SESSION,
      "hello from phone",
      HOST,
    ),
  );
});

describe("private session view", () => {
  it.each(["refused", "transport"] as const)(
    "offers access setup only for an authorization error in history: %s",
    async (code) => {
      vi.mocked(hostControl.privateRead).mockRejectedValue(
        new HostControlError("history unavailable", code),
      );
      view();
      expect(await screen.findByText(/history unavailable/)).toBeVisible();
      if (code === "refused") {
        expect(
          screen.getByRole("link", { name: "Set up access" }),
        ).toHaveAttribute("href", "/settings");
        expect(
          screen.queryByRole("button", { name: "Retry history" }),
        ).toBeNull();
      } else {
        expect(
          screen.queryByRole("link", { name: "Set up access" }),
        ).toBeNull();
        expect(
          screen.getByRole("button", { name: "Retry history" }),
        ).toBeVisible();
      }
      expect(
        screen.queryByText("Private fixture content"),
      ).not.toBeInTheDocument();
      expect(api.setSharing).not.toHaveBeenCalled();
      expect(api.searchUsers).not.toHaveBeenCalled();
    },
  );

  it("shows host-discovered PR links without querying central PR metadata", async () => {
    view();
    const link = await screen.findByRole("link", {
      name: "acme/private-shell#23",
    });
    expect(link).toHaveAttribute(
      "href",
      "https://github.com/acme/private-shell/pull/23",
    );
  });

  it("renders structured messages and picks up a rewrite of an earlier one", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    put(
      message("u1", "assistant", [
        {
          type: "tool_use",
          id: "toolu_1",
          name: "Bash",
          input: { command: "cargo test" },
        },
      ]),
    );
    put(message("u2", "user", [{ type: "text", text: "anything else?" }]));
    view();
    expect((await screen.findAllByText(/Bash/)).length).toBeGreaterThan(0);
    expect(screen.getByText("anything else?")).toBeInTheDocument();
    // The tool result lands IN message u1 (same seq): a seq-only reader would miss it.
    put(
      message("u1", "assistant", [
        {
          type: "tool_use",
          id: "toolu_1",
          name: "Bash",
          input: { command: "cargo test" },
        },
        { type: "text", text: "all green" },
      ]),
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5_000);
    });
    expect(await screen.findByText("all green")).toBeInTheDocument();
  });

  it("searches and replies only through the host", async () => {
    put(message("u1", "user", [{ type: "text", text: "hello" }]));
    view();
    await screen.findByText("hello");
    const user = userEvent.setup();
    await user.type(screen.getByLabelText("Search this session"), "cargo");
    await user.click(screen.getByRole("button", { name: "Search" }));
    expect(await screen.findByText("cargo test matched")).toBeInTheDocument();
    await user.type(screen.getByLabelText("Reply"), "run it again");
    await user.click(screen.getByRole("button", { name: "Send" }));
    await waitFor(() =>
      expect(hostControl.sendReply).toHaveBeenCalledWith(
        SESSION,
        "run it again",
        HOST,
      ),
    );
  });

  it("shares read-only from one Share dialog and warns that typing stays blocked until a key is trusted", async () => {
    put(message("u1", "user", [{ type: "text", text: "hello" }]));
    view();
    await screen.findByText("hello");
    const user = userEvent.setup();
    // One Share… action replaces "Publish history" and "Key access" (EX-01).
    await user.click(screen.getByRole("button", { name: "Share…" }));
    // The host, not the server, says what is published now.
    await waitFor(() =>
      expect(hostControl.publicationStatus).toHaveBeenCalledWith(HOST, SESSION),
    );
    // Nothing is published yet, so there is nothing to stop.
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: "Share read-only" }),
      ).toBeEnabled(),
    );
    expect(screen.getByRole("button", { name: "Stop sharing" })).toBeDisabled();
    await user.click(
      screen.getByRole("radio", { name: /entire organization/i }),
    );
    await user.click(screen.getByRole("button", { name: "Share read-only" }));
    await waitFor(() =>
      expect(hostControl.sharePublication).toHaveBeenCalledWith(HOST, SESSION, {
        visibility: "organization",
      }),
    );
    expect(
      await screen.findByText("Syncing", {
        selector: "[data-publication-state]",
      }),
    ).toBeInTheDocument();
    // REQ-003 / EX-01: the sharer is told typing stays blocked until a key
    // is trusted.
    expect(await screen.findByRole("status")).toHaveTextContent(
      /can't type into it until you trust their key/i,
    );
    // Sharing read-only is not key access, and never the legacy central writer.
    expect(hostControl.addGrant).not.toHaveBeenCalled();
    expect(api.setSharing).not.toHaveBeenCalled();
  });

  it("marks a reader as intended to write, pending key trust (EX-02)", async () => {
    put(message("u1", "user", [{ type: "text", text: "hello" }]));
    view();
    await screen.findByText("hello");
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "Share…" }));
    vi.mocked(hostControl.passkeyInvite).mockResolvedValue(
      "https://conversations.example.com/host-link#desc=chd1.demo",
    );
    // Specific people is the default; a colleague without a trusted key
    // reads only, and is offered an invite to type.
    await user.type(
      await screen.findByLabelText("Add people by email"),
      "alice@example.com{Enter}",
    );
    expect(
      await screen.findByText("Read-only · key not trusted"),
    ).toBeInTheDocument();
    await user.click(
      screen.getByRole("button", { name: "Invite alice@example.com to type" }),
    );
    await screen.findByLabelText("Invite message");
    await user.click(screen.getByRole("button", { name: "Share read-only" }));
    await waitFor(() =>
      expect(hostControl.sharePublication).toHaveBeenCalledWith(HOST, SESSION, {
        visibility: "restricted",
        emails: ["alice@example.com"],
        write_pending: ["alice@example.com"],
      }),
    );
    // EX-02: the sharer is told what stays blocked and why.
    expect(await screen.findByRole("status")).toHaveTextContent(
      /stay read-only until you trust their key/i,
    );
  });

  it("invites a pending writer and trusts their offered key in the browser (EX-03/EX-04)", async () => {
    vi.mocked(hostControl.passkeyInvite).mockResolvedValue(
      "https://conversations.example.com/host-link#desc=chd1.demo&challenge=0123456789abcdef0123456789abcdef&options=eyJwdWJsaWNLZXkiOnt9fQ",
    );
    let offered = false;
    vi.mocked(hostControl.passkeyInvite).mockImplementation(async () => {
      // The colleague completes the passkey against the invite after it is
      // created: from then on the host holds their key offer.
      offered = true;
      return "https://conversations.example.com/host-link#desc=chd1.demo&challenge=0123456789abcdef0123456789abcdef&options=eyJwdWJsaWNLZXkiOnt9fQ";
    });
    vi.mocked(hostControl.passkeyApprove).mockImplementation(async () => {
      offered = false;
    });
    // The colleague's offer is vouched by their own host (EX-04): the dialog
    // derives the pairwise code from this browser's pinned host key and the
    // vouching host's key.
    vi.mocked(hostControl.passkeyPending).mockImplementation(async () =>
      offered
        ? [
            {
              cred_id: "C".repeat(43),
              label: "alice@example.com",
              kind: "agent",
              grants: [`session.write:${SESSION}`, `history.read:${SESSION}`],
              vouch_host_id: "V".repeat(43),
              vouch_host_key: "V".repeat(43),
            },
          ]
        : [],
    );
    put(message("u1", "user", [{ type: "text", text: "hello" }]));
    view();
    await screen.findByText("hello");
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "Share…" }));
    await user.type(
      await screen.findByLabelText("Add people by email"),
      "alice@example.com{Enter}",
    );
    // The invite is scoped to exactly this session.
    await user.click(
      await screen.findByRole("button", {
        name: "Invite alice@example.com to type",
      }),
    );
    await waitFor(() =>
      expect(hostControl.passkeyInvite).toHaveBeenCalledWith(
        HOST,
        "alice@example.com",
        [`session.write:${SESSION}`, `history.read:${SESSION}`],
      ),
    );
    const inviteBox = (await screen.findByLabelText(
      "Invite message",
    )) as HTMLTextAreaElement;
    expect(inviteBox.value).toContain("/host-link#desc=");
    // Copying hands over the explanation with the link, and says it did.
    await user.click(screen.getByRole("button", { name: "Copy invite" }));
    expect(
      await screen.findByRole("button", { name: "Copied ✓" }),
    ).toBeInTheDocument();
    const copied = await navigator.clipboard.readText();
    expect(copied).toMatch(/create a passkey/i);
    expect(copied).toContain("/host-link#desc=");
    // The colleague's offer arrives while the invite dialog is still open.
    await user.click(
      screen.getByRole("button", { name: "Check for their key" }),
    );
    expect(
      await screen.findByText("Keys waiting for your approval"),
    ).toBeInTheDocument();
    // The offer carries the vouching host: the pairwise code is derived from
    // the pinned host key and the vouch key (EX-04).
    expect(await screen.findByText(/Pair code:/)).toBeInTheDocument();
    // Trusting is gated on the comparison: the owner confirms first (U-7).
    expect(
      screen.getByRole("button", { name: "Trust this key" }),
    ).toBeDisabled();
    await user.click(
      screen.getByRole("checkbox", {
        name: /Confirm this key: alice@example\.com/,
      }),
    );
    await user.click(screen.getByRole("button", { name: "Trust this key" }));
    await waitFor(() =>
      expect(hostControl.passkeyApprove).toHaveBeenCalledWith(
        HOST,
        "C".repeat(43),
      ),
    );
    // The host records the trust (PRI-001); the owner is told what it means.
    expect(await screen.findByRole("status")).toHaveTextContent(/Key trusted/i);
  });

  it("offers no trust for your own device's key and points to where it is approved", async () => {
    vi.mocked(hostControl.passkeyPending).mockResolvedValue([
      {
        cred_id: "P".repeat(43),
        label: "Conversations phone",
        kind: "owner",
        grants: ["history.read", "session.write"],
      },
    ]);
    put(message("u1", "user", [{ type: "text", text: "hello" }]));
    view();
    await screen.findByText("hello");
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "Share…" }));
    expect(
      await screen.findByText(/Conversations phone” is waiting for approval/),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("link", { name: "Settings → Access" }),
    ).toHaveAttribute("href", "/settings");
    expect(screen.queryByRole("button", { name: "Trust this key" })).toBeNull();
  });

  it("shows who can type, and lets a trusted key type here without a new invite", async () => {
    vi.mocked(hostControl.addGrant).mockResolvedValue(undefined);
    vi.mocked(hostControl.listAccess).mockResolvedValue([
      {
        kid: "A".repeat(43),
        label: "alice@example.com",
        alg: "es256",
        kind: "agent",
        revoked: false,
        grants: [{ cap: "session.write", session: SESSION }],
      },
      {
        kid: "B".repeat(43),
        label: "bob@example.com",
        alg: "es256",
        kind: "agent",
        revoked: false,
        grants: [{ cap: "session.write", session: "other-session" }],
      },
      // An owner device is never a person, whatever its label says.
      {
        kid: "O".repeat(43),
        label: "carol@example.com",
        alg: "es256",
        kind: "owner",
        revoked: false,
        grants: [{ cap: "session.write", session: "*" }],
      },
      // Two keys under one name: nothing to pick silently.
      ...["D", "E"].map((c) => ({
        kid: c.repeat(43),
        label: "dave@example.com",
        alg: "es256",
        kind: "agent",
        revoked: false,
        grants: [],
      })),
    ]);
    put(message("u1", "user", [{ type: "text", text: "hello" }]));
    view();
    await screen.findByText("hello");
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "Share…" }));
    const input = await screen.findByLabelText("Add people by email");
    await user.type(input, "alice@example.com{Enter}");
    await user.type(input, "bob@example.com{Enter}");
    await user.type(input, "carol@example.com{Enter}");
    await user.type(input, "dave@example.com{Enter}");
    expect(
      within(await screen.findByTestId("person-alice@example.com")).getByText(
        "Can type",
      ),
    ).toBeInTheDocument();
    const carol = screen.getByTestId("person-carol@example.com");
    expect(within(carol).queryByText("Can type")).toBeNull();
    expect(
      within(carol).getByRole("button", {
        name: "Invite carol@example.com to type",
      }),
    ).toBeInTheDocument();
    const dave = screen.getByTestId("person-dave@example.com");
    expect(within(dave).getByText(/Several keys/)).toBeInTheDocument();
    expect(within(dave).queryByRole("button")).toBeNull();
    const bob = screen.getByTestId("person-bob@example.com");
    expect(within(bob).getByText(/key BBBBBBBB…/)).toBeInTheDocument();
    await user.click(within(bob).getByRole("button", { name: "Allow typing" }));
    await waitFor(() =>
      expect(hostControl.addGrant).toHaveBeenCalledWith(
        SESSION,
        "B".repeat(43),
        ["history.read", "session.write"],
        HOST,
      ),
    );
    expect(hostControl.passkeyInvite).not.toHaveBeenCalled();
  });

  it("picks up a key that arrives after the dialog was reopened", async () => {
    held = statusOf("shared", {
      visibility: "restricted",
      emails: ["alice@example.com"],
      write_pending: ["alice@example.com"],
    });
    let offered = false;
    vi.mocked(hostControl.passkeyPending).mockImplementation(async () =>
      offered
        ? [
            {
              cred_id: "C".repeat(43),
              label: "alice@example.com",
              kind: "agent",
              grants: [`session.write:${SESSION}`],
            },
          ]
        : [],
    );
    put(message("u1", "user", [{ type: "text", text: "hello" }]));
    view();
    await screen.findByText("hello");
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "Share…" }));
    await screen.findByText("Read-only · key not trusted");
    offered = true;
    await user.click(
      screen.getByRole("button", { name: "Check for new keys" }),
    );
    expect(
      await screen.findByText("Key offered · confirm it below"),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Trust this key" }),
    ).toBeInTheDocument();
  });

  it("stops sharing read-only and shows the deletion as pending until the host confirms it", async () => {
    held = statusOf("shared", { visibility: "organization" });
    put(message("u1", "user", [{ type: "text", text: "hello" }]));
    view();
    await screen.findByText("hello");
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "Share…" }));
    await screen.findByText("Shared", { selector: "[data-publication-state]" });
    await user.click(screen.getByRole("button", { name: "Stop sharing" }));
    await waitFor(() =>
      expect(hostControl.unsharePublication).toHaveBeenCalledWith(
        HOST,
        SESSION,
      ),
    );
    expect(
      await screen.findByText("Deletion pending", {
        selector: "[data-publication-state]",
      }),
    ).toBeInTheDocument();
  });

  it("grants write to an approved key from the same Share dialog", async () => {
    vi.mocked(hostControl.addGrant).mockResolvedValue(undefined);
    vi.mocked(hostControl.removeGrant).mockResolvedValue(undefined);
    put(message("u1", "user", [{ type: "text", text: "hello" }]));
    view();
    await screen.findByText("hello");
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "Share…" }));
    expect(screen.getByLabelText("Share a session")).toBeInTheDocument();
    expect(
      screen.getByRole("checkbox", { name: "Read this session’s history" }),
    ).toBeChecked();
    await user.click(
      screen.getByRole("checkbox", { name: "Write to this session" }),
    );
    await user.type(
      await screen.findByLabelText("Credential id"),
      "C".repeat(43),
    );
    await user.click(
      screen.getByRole("button", { name: "Allow selected access" }),
    );
    await waitFor(() =>
      expect(hostControl.addGrant).toHaveBeenCalledWith(
        SESSION,
        "C".repeat(43),
        ["history.read", "session.write"],
        HOST,
      ),
    );
    const credential = screen.getByLabelText("Credential id");
    await waitFor(() => expect(credential).toHaveValue(""));
    await user.type(credential, "C".repeat(43));
    await user.click(screen.getByRole("button", { name: "Remove access" }));
    await waitFor(() =>
      expect(hostControl.removeGrant).toHaveBeenCalledWith(
        SESSION,
        "C".repeat(43),
        ["history.read", "session.write"],
        HOST,
      ),
    );
    // The write grant is not publication: nothing reached the server's copy.
    expect(hostControl.sharePublication).not.toHaveBeenCalled();
    expect(api.setSharing).not.toHaveBeenCalled();
  });

  it("binds to one host: the same session id on another paired host is read afresh from it", async () => {
    put(message("u1", "user", [{ type: "text", text: "from the first host" }]));
    view();
    await screen.findByText("from the first host");
    const user = userEvent.setup();
    await user.type(screen.getByLabelText("Reply"), "half typed");
    // Changing the launch host keeps this session open until explicit navigation.
    const other = await makeTestHost();
    await act(async () => {
      await pairHost(other.descriptor, "desktop");
    });
    await userEvent.click(
      screen.getByRole("link", { name: "Open selected host session" }),
    );
    expect(await screen.findByText("from the other host")).toBeInTheDocument();
    expect(screen.queryByText("from the first host")).toBeNull();
    expect(screen.getByLabelText("Reply")).toHaveValue("");
    expect(vi.mocked(hostControl.privateRead).mock.calls.at(-1)?.[0]).toBe(
      other.id,
    );
    await user.type(screen.getByLabelText("Reply"), "to the other host");
    await user.click(screen.getByRole("button", { name: "Send" }));
    await waitFor(() =>
      expect(hostControl.sendReply).toHaveBeenCalledWith(
        SESSION,
        "to the other host",
        other.id,
      ),
    );
    await user.click(screen.getByRole("button", { name: "Share…" }));
    await waitFor(() =>
      expect(hostControl.publicationStatus).toHaveBeenCalledWith(
        other.id,
        SESSION,
      ),
    );
    expect(hostControl.publicationStatus).not.toHaveBeenCalledWith(
      HOST,
      SESSION,
    );
  });
});

it("offers an explicit unlock on a locked fullscreen private link", async () => {
  sessionRunning = true;
  vi.mocked(hostControl.privateRead).mockRejectedValue(
    new HostControlError("Device is locked", "locked"),
  );
  vi.mocked(unlockAgain).mockImplementation(async () => {
    vi.mocked(hostControl.privateRead).mockImplementation(((
      _hostId: string,
      read: (c: PrivateChannel) => unknown,
    ) => Promise.resolve(read(channel))) as never);
  });
  renderWithProviders(
    <>
      <SwitchHostLink />
      <Routes>
        <Route path="/private/:id" element={<PrivateSessionView />} />
      </Routes>
    </>,
    { route: `/private/${SESSION}?fullscreen=1` },
  );
  const button = await screen.findByRole("button", {
    name: "Unlock with passkey",
  });
  expect(unlockAgain).not.toHaveBeenCalled();
  await userEvent.click(button);
  expect(await screen.findByTestId("live-terminal")).toBeVisible();
  expect(unlockAgain).toHaveBeenCalledWith(HOST);
});

describe("private terminal", () => {
  beforeEach(() => {
    sessionRunning = true;
  });
  it("follows a running session's terminal through the live renderer, from this host only", async () => {
    put(message("u1", "user", [{ type: "text", text: "hello" }]));
    renderWithProviders(
      <Routes>
        <Route path="/private" element={<PrivateSessionList />} />
        <Route path="/private/:id" element={<PrivateSessionView />} />
      </Routes>,
      { route: "/private" },
    );
    const user = userEvent.setup();
    await user.click(await screen.findByText("Fix the build"));
    await userEvent.click(
      await screen.findByRole("button", { name: /Show full conversation/ }),
    );
    await screen.findByText("hello");
    expect(screen.getByTestId("live-terminal")).toBeInTheDocument();
    expect(screen.getByTestId("live-terminal")).toBeInTheDocument();
    expect(terminalProps.at(-1)).toMatchObject({
      id: SESSION,
      agent: "claude",
      privateHostId: HOST,
    });
    // Changing the launch host leaves the current terminal on its original host.
    const other = await makeTestHost();
    await act(async () => {
      await pairHost(other.descriptor, "desktop");
    });
    expect(screen.getByTestId("live-terminal")).toBeInTheDocument();
    expect(terminalProps.every((p) => p.privateHostId === HOST)).toBe(true);
  });

  it("offers the terminal when opened directly, from the host's own list", async () => {
    put(message("u1", "user", [{ type: "text", text: "hello" }]));
    view();
    await userEvent.click(
      await screen.findByRole("button", { name: /Show full conversation/ }),
    );
    await screen.findByText("hello");
    await waitFor(() =>
      expect(screen.getByTestId("live-terminal")).toBeInTheDocument(),
    );
  });

  it("offers no terminal for a session the host does not report running", async () => {
    sessionRunning = false;
    put(message("u1", "user", [{ type: "text", text: "hello" }]));
    view();
    await screen.findByText("hello");
    await waitFor(() =>
      expect(vi.mocked(hostControl.privateRead)).toHaveBeenCalled(),
    );
    await new Promise((r) => setTimeout(r, 20));
    expect(screen.queryByTestId("live-terminal")).toBeNull();
  });

  it("renders the live terminal above PR metadata so the running agent leads the page", async () => {
    put(message("u1", "user", [{ type: "text", text: "hello" }]));
    view();
    const terminal = await screen.findByTestId("live-terminal");
    const prLink = await screen.findByRole("link", {
      name: "acme/private-shell#23",
    });
    expect(
      terminal.compareDocumentPosition(prLink) &
        Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
  });

  // Fullscreen first-paint contract for the private view: a `?fullscreen=1`
  // deep link must never flash the normal detail header, PR / Linear cards,
  // or reply composer while the host's session list is still in flight. The
  // shell renders from the first paint, and only when the host reports the
  // session running does the terminal take over.
  it("renders only the fullscreen shell while the host list is deferred", async () => {
    sessionRunning = true;
    // Hold the host's sessions list in flight — this leaves `entry` undefined
    // just like a cold deep link on a page with no cached list yet.
    vi.mocked(hostControl.privateRead).mockImplementation(((
      _hostId: string,
      read: (c: PrivateChannel) => unknown,
    ) => {
      // Treat the sessions-list read differently from every other read by
      // calling it against a stub that never resolves.
      return Promise.race([
        new Promise(() => {}),
        read({
          historyGet: async () => ({
            status: "executed",
            detail: null,
            body: { kind: "history", items: [], next: null },
          }),
          historySearch: async () => ({
            status: "executed",
            detail: null,
            body: { kind: "search", hits: [] },
          }),
          sessionsList: () => new Promise(() => {}),
        } as unknown as PrivateChannel),
      ]);
    }) as never);
    renderWithProviders(
      <Routes>
        <Route path="/private/:id" element={<PrivateSessionView />} />
      </Routes>,
      { route: `/private/${SESSION}?fullscreen=1` },
    );
    expect(await screen.findByTestId("fullscreen-pending")).toBeInTheDocument();
    // No normal-page content should be visible before the terminal is ready.
    expect(screen.queryByTestId("live-terminal")).toBeNull();
    expect(screen.queryByRole("button", { name: /Rename/ })).toBeNull();
    expect(
      screen.queryByRole("button", { name: /Publish history/ }),
    ).toBeNull();
    expect(screen.queryByLabelText(/Reply/i)).toBeNull();
    expect(
      screen.queryByRole("button", { name: /Show full conversation/ }),
    ).toBeNull();
    expect(
      screen.queryByRole("link", { name: "acme/private-shell#23" }),
    ).toBeNull();
  });
});

describe("private session list per host", () => {
  it("keeps each host's list apart in the cache", async () => {
    vi.mocked(hostControl.privateRead).mockImplementation(((
      hostId: string,
      read: (c: PrivateChannel) => unknown,
    ) =>
      hostId === HOST
        ? Promise.resolve(read(channel))
        : Promise.reject(
            new HostControlError("no route", "transport"),
          )) as never);
    renderWithProviders(<PrivateSessionList />);
    expect(await screen.findByText("Fix the build")).toBeInTheDocument();
    const other = await makeTestHost();
    await act(async () => {
      await pairHost(other.descriptor, "desktop");
    });
    // The first host's list is not shown as the second host's.
    expect(
      await screen.findByText("Your host is not reachable privately"),
    ).toBeInTheDocument();
    expect(screen.queryByText("Fix the build")).toBeNull();
  });
});

describe("private card usability", () => {
  it("orders running sessions first and hides only stopped empty sessions", async () => {
    const entry = (
      id: string,
      running: boolean,
      messages: number,
      last_activity: string | null,
    ): SessionEntry => ({
      id,
      agent: "codex",
      cwd: "/Users/me/example",
      title: `${id} task`,
      running,
      messages,
      last_activity,
    });
    listed = [
      entry("Older", false, 2, null),
      entry("Empty", false, 0, null),
      entry("Recent", false, 4, "2026-11-05T10:00:00Z"),
      entry("Running", true, 0, null),
    ];
    renderWithProviders(<PrivateSessionList />);
    await screen.findByText("Running task");
    expect(
      screen
        .getAllByRole("link")
        .filter((a) => !a.closest("details"))
        .map((a) => a.getAttribute("href")),
    ).toEqual([
      "/private/Running?fullscreen=1",
      "/private/Recent",
      "/private/Older",
    ]);
    expect(screen.queryByText("Empty task")).toBeNull();
    await userEvent.click(
      screen.getByText("Stopped sessions without messages (1)"),
    );
    expect(screen.getByText("Empty task")).toBeVisible();
    expect(screen.getByRole("link", { name: /Empty/ })).toHaveAttribute(
      "href",
      "/private/Empty",
    );
  });

  it("renders recent archives in pages while retaining all live sessions", async () => {
    listed = Array.from({ length: 61 }, (_, i) => ({
      id: `archive-${i}`,
      agent: "codex",
      cwd: "/work",
      title: `History ${i}`,
      running: false,
      messages: 1,
      last_activity: null,
    }));
    renderWithProviders(<PrivateSessionList />);
    await screen.findByText("History 0");
    expect(screen.getAllByRole("link")).toHaveLength(24);
    await userEvent.click(
      screen.getByRole("button", { name: /Show more stopped sessions/ }),
    );
    expect(screen.getAllByRole("link")).toHaveLength(48);
  });

  it("offers continuation with imported history using the existing host action", async () => {
    sessionRunning = false;
    resumeMode = "continuation";
    reopenable = true;
    vi.mocked(hostControl.reopenSession).mockResolvedValue({ delivered: true });
    view();
    await userEvent.click(
      await screen.findByRole("button", { name: "Continue locally" }),
    );
    expect(hostControl.reopenSession).toHaveBeenCalledWith(SESSION);
    expect(
      screen.getByText(/new agent session with the imported history/i),
    ).toBeInTheDocument();
  });

  it.each([undefined, true])(
    "reopens with reopenable=%s and waits for host confirmation",
    async (value) => {
      vi.useFakeTimers({ shouldAdvanceTime: true });
      const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
      sessionRunning = false;
      reopenable = value;
      vi.mocked(hostControl.reopenSession).mockResolvedValue({
        delivered: true,
      });
      view();
      await user.click(await screen.findByRole("button", { name: "Reopen" }));
      expect(hostControl.reopenSession).toHaveBeenCalledWith(SESSION);
      expect(screen.getByRole("button", { name: "Reopening…" })).toBeDisabled();
      expect(screen.queryByTestId("live-terminal")).toBeNull();
      sessionRunning = true;
      await act(async () => {
        await vi.advanceTimersByTimeAsync(5_000);
      });
      await waitFor(() =>
        expect(screen.getByTestId("live-terminal")).toBeInTheDocument(),
      );
      expect(screen.queryByRole("button", { name: "Reopening…" })).toBeNull();
    },
  );

  it("shows reopen errors and allows another attempt", async () => {
    sessionRunning = false;
    vi.mocked(hostControl.reopenSession).mockRejectedValue(
      new Error("Host refused reopen"),
    );
    view();
    await userEvent.click(
      await screen.findByRole("button", { name: "Reopen" }),
    );
    expect(await screen.findByText("Host refused reopen")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Reopen" })).toBeEnabled();
  });

  it("explains missing native data and disables writes for read-only entries", async () => {
    sessionRunning = false;
    reopenable = false;
    view();
    expect(
      await screen.findByRole("button", { name: "Reopen" }),
    ).toBeDisabled();
    expect(
      screen.getByText(/native agent data is unavailable/i),
    ).toBeInTheDocument();
    expect(screen.queryByLabelText("Reply")).toBeNull();
    expect(hostControl.reopenSession).not.toHaveBeenCalled();
  });
});

describe("search private sessions locally", () => {
  it.each(["needle", "ARCHIVE-project", "codex"])(
    "finds matches beyond the first rendered page by %s",
    async (query) => {
      listed = Array.from({ length: 30 }, (_, i) => ({
        id: `item-${i}`,
        agent: "claude",
        cwd: "/work/other",
        title: `History ${i}`,
        running: false,
        messages: 1,
        last_activity: null,
      }));
      listed.push({
        id: "last",
        agent: "codex",
        cwd: "/work/archive-project",
        title: "Find the needle",
        running: false,
        messages: 1,
        last_activity: null,
      });
      renderWithProviders(<PrivateSessionList />);
      const search = await screen.findByRole("searchbox", {
        name: "Search private sessions",
      });
      const reads = vi.mocked(hostControl.privateRead).mock.calls.length;
      await userEvent.type(search, query);
      expect(
        screen.getByRole("link", { name: /Find the needle/ }),
      ).toHaveAttribute("href", "/private/last");
      expect(screen.getAllByRole("link")).toHaveLength(1);
      expect(vi.mocked(hostControl.privateRead).mock.calls.length).toBe(reads);
      await userEvent.clear(search);
      expect(screen.getAllByRole("link")).toHaveLength(24);
      await userEvent.type(search, "no-such-session");
      expect(
        screen.getByText("No matching private sessions."),
      ).toBeInTheDocument();
    },
  );
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}
function historyPage(
  text: string,
  seq: number,
  next: number | null,
): ChannelResult {
  return ok({
    kind: "history",
    items: [
      {
        seq,
        uuid: `page-${seq}`,
        size: 1,
        sha256: `hash-${seq}`,
        message: message(`page-${seq}`, "user", [{ type: "text", text }]),
      },
    ],
    next,
  });
}

describe("progressive private session history", () => {
  it("offers terminal controls before page one and displays page one while page two is delayed", async () => {
    sessionRunning = true;
    const first = deferred<ChannelResult>();
    const second = deferred<ChannelResult>();
    const slow = {
      ...channel,
      historyGet: vi.fn((_id: string, after = 0) =>
        after === 0 ? first.promise : second.promise,
      ),
    };
    vi.mocked(hostControl.privateRead).mockImplementation(((
      _host: string,
      read: (c: PrivateChannel) => unknown,
    ) => Promise.resolve(read(slow))) as never);
    view();
    await screen.findByTestId("live-terminal");
    expect(slow.historyGet).not.toHaveBeenCalled();
    await userEvent.click(
      screen.getByRole("button", { name: /Show full conversation/ }),
    );
    await act(async () =>
      first.resolve(historyPage("First complete page", 1, 1)),
    );
    expect(await screen.findByText("First complete page")).toBeInTheDocument();
    expect(screen.getByText(/Loading more history/)).toBeInTheDocument();
    await act(async () =>
      second.resolve(historyPage("Last complete page", 2, null)),
    );
    expect(await screen.findByText("Last complete page")).toBeInTheDocument();
    expect(screen.getByText("First complete page")).toBeInTheDocument();
    expect(screen.queryByText(/Loading more history/)).toBeNull();
  });

  it("retains complete pages and shows the actual failure if a later page fails", async () => {
    const second = deferred<ChannelResult>();
    const slow = {
      ...channel,
      historyGet: vi.fn(async (_id: string, after = 0) =>
        after === 0 ? historyPage("Retained first page", 1, 1) : second.promise,
      ),
    };
    vi.mocked(hostControl.privateRead).mockImplementation(((
      _host: string,
      read: (c: PrivateChannel) => unknown,
    ) => Promise.resolve(read(slow))) as never);
    view();
    await screen.findByText("Retained first page");
    await act(async () => second.reject(new Error("Page two unavailable")));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Page two unavailable",
    );
    expect(screen.getByText("Retained first page")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Reopen" })).toBeEnabled();
  });

  it("discards a delayed page after switching hosts and does not fetch its next page", async () => {
    const late = deferred<ChannelResult>();
    const get = vi.fn(async (_id: string, after = 0) =>
      after === 0 ? historyPage("Old host first page", 1, 1) : late.promise,
    );
    const slow = { ...channel, historyGet: get };
    vi.mocked(hostControl.privateRead).mockImplementation(((
      host: string,
      read: (c: PrivateChannel) => unknown,
    ) =>
      Promise.resolve(read(host === HOST ? slow : otherHostChannel))) as never);
    view();
    await screen.findByText("Old host first page");
    await waitFor(() => expect(get).toHaveBeenCalledTimes(2));
    const other = await makeTestHost();
    await act(async () => {
      await pairHost(other.descriptor, "desktop");
    });
    await userEvent.click(
      screen.getByRole("link", { name: "Open selected host session" }),
    );
    expect(await screen.findByText("from the other host")).toBeInTheDocument();
    await act(async () => late.resolve(historyPage("Stale late page", 2, 2)));
    expect(screen.queryByText("Old host first page")).toBeNull();
    expect(screen.queryByText("Stale late page")).toBeNull();
    expect(get).toHaveBeenCalledTimes(2);
  });
});

it("cancels pending history on a same-host route change before reading another page", async () => {
  const late = deferred<ChannelResult>();
  const get = vi.fn(async (id: string, after = 0) =>
    id !== SESSION
      ? historyPage("New route history", 1, null)
      : after === 0
        ? historyPage("Old route history", 1, 1)
        : late.promise,
  );
  const slow = { ...channel, historyGet: get };
  vi.mocked(hostControl.privateRead).mockImplementation(((
    _host: string,
    read: (c: PrivateChannel) => unknown,
  ) => Promise.resolve(read(slow))) as never);
  renderWithProviders(
    <>
      <Link to="/private/next">Next session</Link>
      <Routes>
        <Route path="/private/:id" element={<PrivateSessionView />} />
      </Routes>
    </>,
    { route: `/private/${SESSION}` },
  );
  await screen.findByText("Old route history");
  await userEvent.click(screen.getByRole("link", { name: "Next session" }));
  await screen.findByText("New route history");
  await act(async () => late.resolve(historyPage("Old late history", 2, 2)));
  expect(screen.queryByText("Old route history")).toBeNull();
  expect(screen.queryByText("Old late history")).toBeNull();
  expect(get.mock.calls.filter(([id]) => id === SESSION)).toHaveLength(2);
});

it("opens a fullscreen private terminal with active sessions before history arrives", async () => {
  listed = [
    {
      id: SESSION,
      title: "Current",
      agent: "codex",
      cwd: "/work",
      messages: 1,
      running: true,
      last_activity: null,
    },
    {
      id: "next",
      title: "Next",
      agent: "claude",
      cwd: "/work",
      messages: 1,
      running: true,
      last_activity: null,
    },
  ];
  const delayed = deferred<ChannelResult>();
  const slow = { ...channel, historyGet: () => delayed.promise };
  vi.mocked(hostControl.privateRead).mockImplementation(((
    _host: string,
    read: (c: PrivateChannel) => unknown,
  ) => Promise.resolve(read(slow))) as never);
  renderWithProviders(
    <>
      <SwitchHostLink />
      <Routes>
        <Route path="/private/:id" element={<PrivateSessionView />} />
      </Routes>
    </>,
    { route: `/private/${SESSION}?fullscreen=1` },
  );
  await screen.findByTestId("live-terminal");
  expect(terminalProps.at(-1)).toMatchObject({
    id: SESSION,
    privateHostId: HOST,
    privateSessions: listed,
  });
  expect(screen.getByRole("link", { name: "← Sessions" })).toHaveAttribute(
    "href",
    "/",
  );
  await userEvent.click(
    screen.getByRole("button", { name: /Show full conversation/ }),
  );
  await act(async () =>
    delayed.resolve(historyPage("Eventually read", 1, null)),
  );
  await screen.findByText("Eventually read");
});

it("does not request history while a cold live session's metadata is pending", async () => {
  sessionRunning = true;
  const metadata = deferred<ChannelResult>();
  const get = vi.fn(async () => historyPage("Lazy body", 1, null));
  const slow = {
    ...channel,
    sessionsList: () => metadata.promise,
    historyGet: get,
  };
  vi.mocked(hostControl.privateRead).mockImplementation(((
    _host: string,
    read: (c: PrivateChannel) => unknown,
  ) => Promise.resolve(read(slow))) as never);
  view();
  expect(screen.queryByTestId("live-terminal")).toBeNull();
  await act(async () => {
    await Promise.resolve();
  });
  expect(get).not.toHaveBeenCalled();
  await act(async () => metadata.resolve(await channel.sessionsList()));
  await screen.findByTestId("live-terminal");
  expect(get).not.toHaveBeenCalled();
  await userEvent.click(
    screen.getByRole("button", { name: /Show full conversation/ }),
  );
  expect(await screen.findByText("Lazy body")).toBeInTheDocument();
  expect(get).toHaveBeenCalledTimes(1);
});

it("polls only active sessions after the archive loads", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  const calls = vi.spyOn(channel, "sessionsList");
  view();
  await screen.findByTestId("private-session");
  await act(async () => {
    await vi.advanceTimersByTimeAsync(16_000);
  });
  expect(calls.mock.calls.filter((call) => !call[2])).toHaveLength(1);
  expect(
    calls.mock.calls.filter((call) => call[2] === true).length,
  ).toBeGreaterThanOrEqual(3);
  calls.mockRestore();
});

it("uses the shared project and activity-sort controls for private cards", async () => {
  listed = [
    {
      id: "older",
      agent: "codex",
      cwd: "/work/one",
      title: "Older task",
      running: false,
      messages: 1,
      last_activity: "2026-11-01T00:00:00Z",
    },
    {
      id: "newer",
      agent: "codex",
      cwd: "/work/two",
      title: "Newer task",
      running: false,
      messages: 1,
      last_activity: "2026-11-05T00:00:00Z",
    },
  ];
  renderWithProviders(<PrivateSessionList />);
  await screen.findByText("Newer task");
  await userEvent.click(screen.getByRole("button", { name: "Sort sessions" }));
  expect(screen.queryByRole("menuitem", { name: /Newest created/ })).toBeNull();
  await userEvent.click(
    screen.getByRole("menuitem", { name: "Least recent activity" }),
  );
  expect(
    screen.getAllByRole("link").map((link) => link.getAttribute("href")),
  ).toEqual(["/private/older", "/private/newer"]);
  await userEvent.click(screen.getByRole("button", { name: "All projects" }));
  await userEvent.click(screen.getByRole("menuitem", { name: /two/ }));
  expect(screen.queryByText("Older task")).toBeNull();
  expect(screen.getByText("Newer task")).toBeVisible();
});

it("opens a live terminal from bounded active metadata while archive loading is pending", async () => {
  sessionRunning = true;
  const original = channel.sessionsList;
  const archive = deferred<ChannelResult>();
  const spy = vi
    .spyOn(channel, "sessionsList")
    .mockImplementation((after, limit, running) =>
      running ? original(after, limit, running) : archive.promise,
    );
  const history = vi.spyOn(channel, "historyGet");
  const rendered = view();
  await screen.findByTestId("live-terminal");
  expect(history).not.toHaveBeenCalled();
  expect(
    screen.getByRole("button", { name: /Show full conversation/ }),
  ).toBeEnabled();
  rendered.unmount();
  spy.mockRestore();
  history.mockRestore();
});

it("ranks a stopped exact title ahead of a running partial match", async () => {
  listed = [
    {
      id: "partial",
      title: "Fix bug later",
      agent: "codex",
      cwd: "/work",
      running: true,
      messages: 2,
      last_activity: "2026-11-05T00:00:00Z",
    },
    {
      id: "exact",
      title: " FIX BUG ",
      agent: "codex",
      cwd: "/work",
      running: false,
      messages: 0,
      last_activity: null,
    },
  ];
  renderWithProviders(<PrivateSessionList />);
  await screen.findByText("Fix bug later");
  await userEvent.type(
    screen.getByRole("searchbox", { name: "Search private sessions" }),
    "fix bug",
  );
  expect(
    screen.getAllByRole("link").map((link) => link.getAttribute("href")),
  ).toEqual(["/private/exact", "/private/partial?fullscreen=1"]);
});

it("clears cached Ready evidence when the active host query fails", async () => {
  const { useHostSessions } = await import("./PrivateSessions");
  listed = [
    {
      id: SESSION,
      agent: "claude",
      cwd: "/work",
      title: "Quiet",
      messages: 1,
      last_activity: null,
      running: true,
      readiness: "ready",
    },
  ];
  function Evidence() {
    const sessions = useHostSessions(HOST);
    const session = sessions.data?.[0];
    return (
      <span>
        {session ? `${session.running}:${session.readiness}` : "loading"}
      </span>
    );
  }
  const { queryClient } = renderWithProviders(<Evidence />);
  expect(await screen.findByText("true:ready")).toBeInTheDocument();
  vi.mocked(hostControl.privateRead).mockRejectedValue(
    new Error("host disconnected"),
  );
  await act(async () => {
    await queryClient.invalidateQueries({
      queryKey: ["private", HOST, "active-sessions"],
    });
  });
  expect(await screen.findByText("true:unknown")).toBeInTheDocument();
});
