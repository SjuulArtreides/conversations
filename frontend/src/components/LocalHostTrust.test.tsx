import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { LocalHostTrust } from "./LocalHostTrust";
import { localRequest, useLocalHost } from "@/lib/localHost";
import { api } from "@/lib/api";
import { hostControl } from "@/lib/hostControl";

vi.mock("@/lib/localHost", () => ({
  localRequest: vi.fn(),
  useLocalHost: vi.fn(),
}));
const first = {
  kid: "A".repeat(43),
  public_key: "BBDifferentFirstPublicKey==",
  alg: "es256" as const,
  label: "Alice browser",
  created_at: "2026-11-05T10:00:00Z",
};
const second = {
  ...first,
  kid: "B".repeat(43),
  public_key: "BBDifferentSecondPublicKey==",
  label: "Alice other browser",
};
const directory = {
  users: [
    {
      email: "alice@example.com",
      display_name: "Alice",
      keys: [first, second],
    },
    {
      email: "bob@example.com",
      display_name: "Bob",
      keys: [],
    },
  ],
};

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(useLocalHost).mockReturnValue({
    local: true,
    authenticated: true,
    error: null,
  });
  vi.mocked(localRequest).mockImplementation(async (path) => {
    if (path === "/api/status")
      return {
        descriptor: "ac-host-v1:public-host-key",
        server: "https://hosted.example",
        credentials: [],
      } as never;
    if (path.startsWith("/api/users?")) return directory as never;
    if (path === "/api/waiting-keys")
      return { email: "me@example.com", keys: [] } as never;
    if (path === "/api/trust") return { kid: first.kid } as never;
    throw new Error("Unexpected local endpoint");
  });
});

async function openAddPerson(user: ReturnType<typeof userEvent.setup>) {
  await user.click(await screen.findByRole("button", { name: "Add person" }));
}

async function chooseFirst(user: ReturnType<typeof userEvent.setup>) {
  await openAddPerson(user);
  await user.click(
    await screen.findByRole("radio", {
      name: "Select Alice browser for alice@example.com",
    }),
  );
}

describe("local host trust in shared Settings", () => {
  it("defaults to a compact view that does not query the published-key directory", async () => {
    renderWithProviders(<LocalHostTrust />);
    await screen.findByText("Access to this host");
    await waitFor(() =>
      expect(localRequest).toHaveBeenCalledWith("/api/status"),
    );
    // Directory + search live behind Add person; the raw host descriptor is
    // tucked into a collapsed disclosure that only appears on demand.
    expect(
      screen.queryByRole("textbox", { name: /Search published keys/ }),
    ).not.toBeInTheDocument();
    expect(localRequest).not.toHaveBeenCalledWith(
      expect.stringMatching(/^\/api\/users\?/),
      expect.anything(),
    );
    // The connection details disclosure is present but closed by default.
    const connectionDetails = screen
      .getByText("Connection details")
      .closest("details");
    expect(connectionDetails).not.toBeNull();
    expect(connectionDetails?.hasAttribute("open")).toBe(false);
  });

  it("defaults new trust to history.read only and never silently grants session.write", async () => {
    const user = userEvent.setup();
    renderWithProviders(<LocalHostTrust />);
    await chooseFirst(user);
    expect(
      screen.getByRole("button", { name: "Trust public key" }),
    ).toBeDisabled();
    // The old UI defaulted to [history.read, session.pr_sync] so a
    // newly-trusted key could refresh pull requests without controlling
    // the terminal. Now that refresh follows session.write, that checkbox
    // is gone — but the default must NOT silently roll "refresh" into
    // "terminal control". The initial grant set is history.read only.
    expect(
      screen.getByRole("checkbox", { name: "Read session history" }),
    ).toBeChecked();
    expect(
      screen.getByRole("checkbox", {
        name: /Control running terminals/,
      }),
    ).not.toBeChecked();
    expect(
      screen.queryByRole("checkbox", { name: /Refresh pull-request/ }),
    ).not.toBeInTheDocument();
    await user.click(
      screen.getByRole("checkbox", { name: /I independently compared/ }),
    );
    await user.click(screen.getByRole("button", { name: "Trust public key" }));
    await screen.findByRole("status");
    const call = vi
      .mocked(localRequest)
      .mock.calls.find(([path]) => path === "/api/trust")!;
    expect(call[1]?.method).toBe("POST");
    expect(JSON.parse(String(call[1]?.body))).toEqual({
      kid: first.kid,
      public_key: first.public_key,
      label: "alice@example.com: Alice browser",
      compared: true,
      grants: ["history.read"],
    });
    expect(
      screen.getByRole("button", { name: "Trust public key" }),
    ).toBeDisabled();
  });

  it("writes session.write (and therefore PR refresh) only after explicit opt-in", async () => {
    const user = userEvent.setup();
    renderWithProviders(<LocalHostTrust />);
    await chooseFirst(user);
    await user.click(
      screen.getByRole("checkbox", { name: /Control running terminals/ }),
    );
    await user.click(
      screen.getByRole("checkbox", { name: /I independently compared/ }),
    );
    await user.click(screen.getByRole("button", { name: "Trust public key" }));
    await screen.findByRole("status");
    const call = vi
      .mocked(localRequest)
      .mock.calls.find(([path]) => path === "/api/trust")!;
    expect(JSON.parse(String(call[1]?.body)).grants).toEqual([
      "history.read",
      "session.write",
    ]);
  });

  it("hides directory candidates that have no published browser keys", async () => {
    const user = userEvent.setup();
    renderWithProviders(<LocalHostTrust />);
    await openAddPerson(user);
    await screen.findByRole("radio", {
      name: "Select Alice browser for alice@example.com",
    });
    expect(screen.queryByText("bob@example.com")).not.toBeInTheDocument();
    expect(
      screen.queryByText(/No published browser keys\./),
    ).not.toBeInTheDocument();
  });

  it.each([true, false])(
    "adds only the selected owner permission and reports whether it was new (%s)",
    async (newPermission) => {
      vi.mocked(localRequest).mockImplementation(async (path) => {
        if (path === "/api/status")
          return {
            descriptor: "ac-host-v1:public-host-key",
            server: "https://hosted.example",
            credentials: [
              {
                kid: first.kid,
                label: "Legacy owner key",
                revoked: false,
                grants: [
                  { cap: "history.read", session: "*" },
                  { cap: "session.write", session: "*" },
                  { cap: "session.start", session: "*" },
                  { cap: "session.reopen", session: "*" },
                ],
              },
            ],
          } as never;
        if (path.startsWith("/api/users?")) return directory as never;
        if (path === "/api/trust/upgrade")
          return {
            kid: first.kid,
            added: newPermission ? ["passkey.manage"] : [],
          } as never;
        throw new Error("Unexpected local endpoint");
      });
      const user = userEvent.setup();
      renderWithProviders(<LocalHostTrust />);
      // PR refresh no longer appears as a standalone upgrade option: the
      // legacy four-cap owner already holds session.write, which authorizes
      // refresh. The only remaining upgrades are PR comments, sharing and
      // launch settings.
      const details = await screen.findByText("Add permissions");
      expect(details.closest("details")).not.toHaveAttribute("open");
      await user.click(details);
      expect(
        screen.queryByRole("button", {
          name: /Refresh pull-request/,
        }),
      ).not.toBeInTheDocument();
      expect(
        screen.getByRole("button", {
          name: "Allow: Change host launch settings",
        }),
      ).toBeDisabled();
      expect(
        screen.getByText(
          "To change host launch settings, first allow reading them.",
        ),
      ).toBeVisible();
      await user.click(
        screen.getByRole("button", {
          name: "Allow: Invite and approve session keys",
        }),
      );
      expect(await screen.findByRole("status")).toHaveTextContent(
        newPermission
          ? "Permission added: Invite and approve session keys"
          : "Already allowed: Invite and approve session keys",
      );
      const call = vi
        .mocked(localRequest)
        .mock.calls.find(([path]) => path === "/api/trust/upgrade")!;
      expect(call[1]?.method).toBe("POST");
      expect(JSON.parse(String(call[1]?.body))).toEqual({
        kid: first.kid,
        grants: ["passkey.manage"],
      });
      // Touch ID renewal guidance stays alongside the upgrade.
      expect(screen.getByText(/passkey \(Touch ID\)/)).toBeInTheDocument();
    },
  );

  it("resets comparison and elevated permissions when another exact key is selected", async () => {
    const user = userEvent.setup();
    renderWithProviders(<LocalHostTrust />);
    await chooseFirst(user);
    // Pick a cap that is NOT in the on-by-default set so flipping it and
    // then switching keys proves the reset: pr_comment is an owner-only
    // extra, not a default grant.
    await user.click(
      screen.getByRole("checkbox", { name: "Post pull-request comments" }),
    );
    await user.click(
      screen.getByRole("checkbox", { name: /I independently compared/ }),
    );
    await user.click(
      screen.getByRole("radio", {
        name: "Select Alice other browser for alice@example.com",
      }),
    );
    expect(
      screen.getByRole("checkbox", { name: /I independently compared/ }),
    ).not.toBeChecked();
    expect(
      screen.getByRole("checkbox", { name: "Post pull-request comments" }),
    ).not.toBeChecked();
    expect(
      screen.getByRole("button", { name: "Trust public key" }),
    ).toBeDisabled();
    expect(localRequest).not.toHaveBeenCalledWith(
      "/api/trust",
      expect.anything(),
    );
  });

  it("shows trust failures without claiming approval", async () => {
    const original = vi.mocked(localRequest).getMockImplementation()!;
    vi.mocked(localRequest).mockImplementation(async (path, init) => {
      if (path === "/api/trust")
        throw new Error(
          "This key is already enrolled. Its existing permissions were not changed.",
        );
      return original(path, init);
    });
    const user = userEvent.setup();
    renderWithProviders(<LocalHostTrust />);
    await chooseFirst(user);
    await user.click(
      screen.getByRole("checkbox", { name: /I independently compared/ }),
    );
    await user.click(screen.getByRole("button", { name: "Trust public key" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "existing permissions were not changed",
    );
    expect(
      screen.queryByText("Public key trusted with the selected permissions."),
    ).not.toBeInTheDocument();
  });

  it("does not load directory or host data without a local owner cookie", async () => {
    vi.mocked(useLocalHost).mockReturnValue({
      local: true,
      authenticated: false,
      error: null,
    });
    renderWithProviders(<LocalHostTrust />);
    await waitFor(() => expect(localRequest).not.toHaveBeenCalled());
    expect(screen.queryByText("Access to this host")).not.toBeInTheDocument();
  });
});

it("submits only the compared published key and selected permissions for the chosen remote host", async () => {
  vi.mocked(useLocalHost).mockReturnValue({ local: false, authenticated: false, error: null });
  const lookup = vi.spyOn(api, "userKeyDirectory").mockResolvedValue(directory.users);
  const trust = vi.spyOn(hostControl, "trustPublishedBrowser").mockResolvedValue({ kid: first.kid });
  try {
    const user = userEvent.setup();
    renderWithProviders(<LocalHostTrust remoteHost={{ id: "mac", label: "Macbook" }} />);
    await chooseFirst(user);
    expect(localRequest).not.toHaveBeenCalled();
    expect(screen.getByRole("heading", { name: "Browser access to Macbook" })).toBeInTheDocument();
    expect(screen.queryByRole("checkbox", { name: "Approve browser keys remotely with a passkey" })).not.toBeInTheDocument();
    const submit = screen.getByRole("button", { name: "Trust public key" });
    expect(submit).toBeDisabled();
    await user.click(screen.getByRole("checkbox", { name: /independently compared/ }));
    await user.click(submit);
    expect(trust).toHaveBeenCalledWith("mac", {
      kid: first.kid, public_key: first.public_key,
      label: "alice@example.com: Alice browser", compared: true,
      grants: ["history.read"],
    });
  } finally {
    lookup.mockRestore();
    trust.mockRestore();
  }
});

describe("browser keys waiting for approval", () => {
  it("lists your account's unapproved fingerprints and approves one with full access in a single click", async () => {
    const user = userEvent.setup();
    const base = vi.mocked(localRequest).getMockImplementation()!;
    vi.mocked(localRequest).mockImplementation(async (path, init) => {
      if (path === "/api/waiting-keys")
        return { email: "me@example.com", keys: [first] } as never;
      return base(path, init);
    });
    renderWithProviders(<LocalHostTrust />);

    expect(await screen.findByText("Waiting for approval")).toBeInTheDocument();
    expect(screen.getByText(/signed in as me@example.com/)).toBeInTheDocument();
    expect(screen.getAllByText(first.kid)[0]).toBeInTheDocument();
    expect(localRequest).not.toHaveBeenCalledWith(
      "/api/trust",
      expect.anything(),
    );

    // One click: the fingerprint is on screen and the key is your own.
    await user.click(screen.getByRole("button", { name: "Approve" }));

    await waitFor(() =>
      expect(localRequest).toHaveBeenCalledWith(
        "/api/trust",
        expect.objectContaining({ method: "POST" }),
      ),
    );
    const call = vi
      .mocked(localRequest)
      .mock.calls.find(([path]) => path === "/api/trust")!;
    const body = JSON.parse(String(call[1]?.body));
    expect(body).toMatchObject({
      kid: first.kid,
      public_key: first.public_key,
      label: first.label,
      compared: true,
    });
    expect([...body.grants].sort()).toEqual(
      [
        "browser_trust.manage",
        "history.read",
        "host.settings_read",
        "host.settings_write",
        "passkey.manage",
        "session.manage_sharing",
        "session.pr_comment",
        "session.reopen",
        "session.start",
        "session.write",
      ],
    );
    expect(await screen.findByRole("status")).toHaveTextContent(
      `Approved ${first.label}`,
    );
  });

  it("gives an approved key every missing permission in one click", async () => {
    const user = userEvent.setup();
    const base = vi.mocked(localRequest).getMockImplementation()!;
    vi.mocked(localRequest).mockImplementation(async (path, init) => {
      if (path === "/api/status")
        return {
          descriptor: "ac-host-v1:public-host-key",
          server: "https://hosted.example",
          credentials: [
            {
              kid: first.kid,
              label: "Legacy owner key",
              revoked: false,
              grants: [
                { cap: "history.read", session: "*" },
                { cap: "session.write", session: "*" },
                { cap: "session.pr_comment", session: "*" },
              ],
            },
          ],
        } as never;
      if (path === "/api/trust/upgrade")
        return { kid: first.kid, added: ["passkey.manage"] } as never;
      return base(path, init);
    });
    renderWithProviders(<LocalHostTrust />);

    await user.click(
      await screen.findByRole("button", { name: "Give full access" }),
    );
    expect(await screen.findByRole("status")).toHaveTextContent(
      "Full access given",
    );
    const call = vi
      .mocked(localRequest)
      .mock.calls.find(([path]) => path === "/api/trust/upgrade")!;
    expect(JSON.parse(String(call[1]?.body))).toEqual({
      kid: first.kid,
      grants: [
        "session.manage_sharing",
        "host.settings_read",
        "host.settings_write",
        "passkey.manage",
        "browser_trust.manage",
      ],
    });
  });

  it("attaches an Approve failure to the row it came from, not the Add person panel", async () => {
    const user = userEvent.setup();
    const base = vi.mocked(localRequest).getMockImplementation()!;
    vi.mocked(localRequest).mockImplementation(async (path, init) => {
      if (path === "/api/waiting-keys")
        return { email: "me@example.com", keys: [first, second] } as never;
      if (path === "/api/trust")
        throw new Error("This key is already enrolled.");
      return base(path, init);
    });
    renderWithProviders(<LocalHostTrust />);
    const firstRow = (
      await screen.findAllByText(first.kid)
    )[0].closest("li")!;
    const secondRow = screen.getAllByText(second.kid)[0].closest("li")!;
    await user.click(
      within(firstRow).getByRole("button", { name: "Approve" }),
    );
    const alert = await within(firstRow).findByRole("alert");
    expect(alert).toHaveTextContent("already enrolled");
    expect(within(secondRow).queryByRole("alert")).not.toBeInTheDocument();
    // The Add person panel was never opened, so no leaked alert there.
    expect(
      screen.queryByText("Public key trusted with the selected permissions."),
    ).not.toBeInTheDocument();
  });

  it("shows nothing extra when every published key is already approved", async () => {
    renderWithProviders(<LocalHostTrust />);
    await waitFor(() =>
      expect(localRequest).toHaveBeenCalledWith("/api/waiting-keys"),
    );
    expect(screen.queryByText("Waiting for approval")).not.toBeInTheDocument();
  });
});
