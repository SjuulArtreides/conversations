import "fake-indexeddb/auto";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  fireEvent,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { Settings } from "./Settings";

vi.mock("@/lib/localHost", async () => {
  const actual =
    await vi.importActual<typeof import("@/lib/localHost")>("@/lib/localHost");
  return { ...actual, useLocalHost: vi.fn(), localRequest: vi.fn() };
});

vi.mock("@/lib/api", async () => {
  const actual = await vi.importActual<typeof import("@/lib/api")>("@/lib/api");
  return {
    ...actual,
    api: {
      ...actual.api,
      authConfig: vi.fn(),
      userKeyDirectory: vi.fn(),
      getUserSettings: vi.fn(),
      setUserSettings: vi.fn(),
      createPasswordResetLink: vi.fn(),
      listAdminUsers: vi.fn(),
      grantAdmin: vi.fn(),
      revokeAdmin: vi.fn(),
      deleteUser: vi.fn(),
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
    hostControl: {
      ...actual.hostControl,
      getHostSettings: vi.fn(),
      updateHostSettings: vi.fn(),
    },
  };
});

import { api } from "@/lib/api";
import { localRequest, useLocalHost } from "@/lib/localHost";
import { hostControl, NOT_APPROVED_MESSAGE } from "@/lib/hostControl";
import {
  pairHost,
  selectHost,
  setHostAuthMode,
  unpairHost,
} from "@/lib/hostPairing";
import { makeTestHost } from "@/lib/testHost";

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(useLocalHost).mockReturnValue({
    local: false,
    authenticated: false,
    error: null,
  });
  vi.mocked(api.userKeyDirectory).mockResolvedValue([]);
  vi.mocked(api.authConfig).mockResolvedValue({
    google_enabled: false,
    password_enabled: true,
    user: {
      email: "dev@example.com",
      display_name: "Dev",
      is_admin: false,
    },
  });
  vi.mocked(api.getUserSettings).mockResolvedValue({
    container_paths: ["/Users/dev/mono"],
    default_agent: "claude",
  });
  vi.mocked(api.setUserSettings).mockImplementation(
    (paths, default_agent, inline_code_colors) =>
      Promise.resolve({
        container_paths: paths,
        default_agent,
        inline_code_colors,
      }),
  );
  vi.mocked(hostControl.getHostSettings).mockResolvedValue({ ...HOST_DEFAULT });
  vi.mocked(hostControl.updateHostSettings).mockResolvedValue(undefined);
  vi.mocked(api.listAdminUsers).mockResolvedValue([]);
});

// The baked-in Claude default the colour picker seeds to → its stored shape.
const DEFAULT_COLORS = { palette: [], rgb: [0xb1b9f9] };

// What a host with nothing overridden answers to a signed settings read. The
// launch flags are the host's; the server never carries them.
const HOST_DEFAULT = {
  claude_skip_permissions: false,
  opencode_auto: false,
  codex_sandbox_mode: null,
  codex_approval_policy: null,
  default_launcher: "opencode" as const,
  adk_statusline: false,
};

/** The launch radios are locked until the host's signed answer arrives. */
async function hostReady() {
  await waitFor(() =>
    expect(
      screen.getByRole("radio", { name: "Use Claude settings" }),
    ).toBeEnabled(),
  );
}

async function renderSettings(section: string) {
  const rendered = renderWithProviders(<Settings />);
  await userEvent.click(await screen.findByRole("button", { name: section }));
  return rendered;
}

describe("Settings — container repositories", () => {
  it("highlights unsaved changes and clears the highlight after reverting or saving", async () => {
    const user = userEvent.setup();
    await renderSettings("Repositories");
    await screen.findByText("/Users/dev/mono");
    const saveButton = screen.getByRole("button", { name: "Save" });
    expect(saveButton).not.toHaveClass("bg-amber-400");

    await user.type(
      screen.getByLabelText("New container repository path"),
      "/srv/other",
    );
    await user.click(screen.getByRole("button", { name: "Add" }));
    expect(saveButton).toHaveClass("bg-amber-400");
    expect(saveButton).toHaveAttribute("title", "Unsaved changes");

    await user.click(
      within(screen.getByText("/srv/other").closest("li")!).getByRole(
        "button",
        { name: "Remove" },
      ),
    );
    expect(saveButton).not.toHaveClass("bg-amber-400");

    await user.click(screen.getByRole("button", { name: "Remove" }));
    expect(saveButton).toHaveClass("bg-amber-400");
    vi.mocked(api.setUserSettings).mockRejectedValueOnce(new Error("Offline"));
    await user.click(saveButton);
    await screen.findByText("Could not save settings: Offline");
    expect(saveButton).toHaveClass("bg-amber-400");

    await user.click(saveButton);
    await screen.findByText("Settings saved");
    await waitFor(() => expect(saveButton).not.toHaveClass("bg-amber-400"));
  });

  it("renders the loaded paths", async () => {
    await renderSettings("Repositories");
    expect(await screen.findByText("/Users/dev/mono")).toBeInTheDocument();
  });

  it("can add a path to the list", async () => {
    const user = userEvent.setup();
    await renderSettings("Repositories");
    await screen.findByText("/Users/dev/mono");

    await user.type(
      screen.getByLabelText("New container repository path"),
      "/srv/other",
    );
    await user.click(screen.getByRole("button", { name: "Add" }));

    expect(await screen.findByText("/srv/other")).toBeInTheDocument();
  });

  it("can remove a path from the list", async () => {
    const user = userEvent.setup();
    await renderSettings("Repositories");
    await screen.findByText("/Users/dev/mono");

    await user.click(screen.getByRole("button", { name: "Remove" }));

    await waitFor(() =>
      expect(screen.queryByText("/Users/dev/mono")).not.toBeInTheDocument(),
    );
  });

  it("Save calls setUserSettings with the edited list", async () => {
    const user = userEvent.setup();
    await renderSettings("Repositories");
    await screen.findByText("/Users/dev/mono");

    // Add one, remove the original, then save.
    await user.type(
      screen.getByLabelText("New container repository path"),
      "/srv/other",
    );
    await user.click(screen.getByRole("button", { name: "Add" }));
    await screen.findByText("/srv/other");

    // Remove the original path (scope to its row to disambiguate).
    const originalRow = screen.getByText("/Users/dev/mono").closest("li")!;
    await user.click(
      within(originalRow).getByRole("button", { name: "Remove" }),
    );

    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() =>
      expect(api.setUserSettings).toHaveBeenCalledWith(
        ["/srv/other"],
        "claude",
        DEFAULT_COLORS,
      ),
    );
  });

  it("can save the default agent", async () => {
    const user = userEvent.setup();
    await renderSettings("Launch");
    await screen.findByText("/Users/dev/mono");

    const agentGroup = screen.getByRole("radiogroup", {
      name: "Default agent",
    });
    await user.click(within(agentGroup).getByRole("radio", { name: "Codex" }));
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() =>
      expect(api.setUserSettings).toHaveBeenCalledWith(
        ["/Users/dev/mono"],
        "codex",
        DEFAULT_COLORS,
      ),
    );
  });

  it("saves adk code as the default: the agent to the account, the launcher to the host", async () => {
    const user = userEvent.setup();
    await renderSettings("Launch");
    const group = await screen.findByRole("radiogroup", {
      name: "Default agent",
    });
    await hostReady();
    await user.click(within(group).getByRole("radio", { name: "adk code" }));
    await user.click(screen.getByRole("button", { name: "Save" }));
    await screen.findByText("Settings saved");
    expect(vi.mocked(api.setUserSettings).mock.calls[0].slice(0, 2)).toEqual([
      ["/Users/dev/mono"],
      "opencode",
    ]);
    expect(hostControl.updateHostSettings).toHaveBeenCalledWith({
      ...HOST_DEFAULT,
      default_launcher: "adk",
    });
  });

  it("restores the saved launcher from the host, not from the account", async () => {
    vi.mocked(api.getUserSettings).mockResolvedValue({
      container_paths: [],
      default_agent: "opencode",
    });
    vi.mocked(hostControl.getHostSettings).mockResolvedValue({
      ...HOST_DEFAULT,
      default_launcher: "adk",
    });
    await renderSettings("Launch");
    await waitFor(() =>
      expect(screen.getByRole("radio", { name: "adk code" })).toBeChecked(),
    );
  });

  it("can save Codex launch overrides", async () => {
    const user = userEvent.setup();
    await renderSettings("Launch");
    await screen.findByText("/Users/dev/mono");

    await user.click(screen.getByRole("button", { name: "Codex" }));
    const sandboxGroup = screen.getByRole("radiogroup", {
      name: "Codex sandbox",
    });
    await user.click(
      within(sandboxGroup).getByRole("radio", {
        name: "Full access",
      }),
    );
    const approvalGroup = screen.getByRole("radiogroup", {
      name: "Codex approval",
    });
    await user.click(
      within(approvalGroup).getByRole("radio", { name: "On request" }),
    );
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() =>
      expect(api.setUserSettings).toHaveBeenCalledWith(
        ["/Users/dev/mono"],
        "claude",
        DEFAULT_COLORS,
      ),
    );
    expect(hostControl.updateHostSettings).toHaveBeenCalledWith({
      ...HOST_DEFAULT,
      codex_sandbox_mode: "danger-full-access",
      codex_approval_policy: "on-request",
    });
  });

  it("saves Claude permission bypass to the host and can restore the default", async () => {
    const user = userEvent.setup();
    await renderSettings("Launch");
    await screen.findByText("/Users/dev/mono");
    await hostReady();
    const group = screen.getByRole("radiogroup", {
      name: "Claude permissions",
    });
    expect(
      within(group).getByRole("radio", { name: "Use Claude settings" }),
    ).toBeChecked();
    await user.click(
      within(group).getByRole("radio", {
        name: "Skip permission prompts",
      }),
    );
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(hostControl.updateHostSettings).toHaveBeenLastCalledWith({
        ...HOST_DEFAULT,
        claude_skip_permissions: true,
      }),
    );
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Save" })).toBeEnabled(),
    );
    await user.click(
      within(group).getByRole("radio", { name: "Use Claude settings" }),
    );
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(hostControl.updateHostSettings).toHaveBeenLastCalledWith(
        HOST_DEFAULT,
      ),
    );
    // The server only ever sees account preferences.
    for (const call of vi.mocked(api.setUserSettings).mock.calls) {
      expect(call).toHaveLength(3);
    }
  });

  it("loads the saved Claude choice from the host and does not rewrite it when saving other settings", async () => {
    vi.mocked(hostControl.getHostSettings).mockResolvedValue({
      ...HOST_DEFAULT,
      claude_skip_permissions: true,
    });
    const user = userEvent.setup();
    await renderSettings("Launch");
    const radio = await screen.findByRole("radio", {
      name: "Skip permission prompts",
    });
    await waitFor(() => expect(radio).toBeChecked());
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(api.setUserSettings).toHaveBeenCalled());
    expect(hostControl.updateHostSettings).not.toHaveBeenCalled();
  });

  it("can start OpenCode with automatic permission approval", async () => {
    const user = userEvent.setup();
    await renderSettings("Launch");
    await screen.findByText("/Users/dev/mono");
    await hostReady();

    await user.click(screen.getByRole("button", { name: "OpenCode & adk" }));
    const approvalGroup = screen.getByRole("radiogroup", {
      name: "OpenCode approval",
    });
    await user.click(
      within(approvalGroup).getByRole("radio", {
        name: "Auto-approve",
      }),
    );
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() =>
      expect(hostControl.updateHostSettings).toHaveBeenCalledWith({
        ...HOST_DEFAULT,
        opencode_auto: true,
      }),
    );
  });

  it("can opt in to the adk code status strip, and no longer offers a central re-open choice", async () => {
    const user = userEvent.setup();
    await renderSettings("Launch");
    await screen.findByText("/Users/dev/mono");
    await hostReady();

    expect(
      screen.queryByRole("radiogroup", { name: "OpenCode re-open command" }),
    ).toBeNull();
    await user.click(screen.getByRole("button", { name: "OpenCode & adk" }));
    const statusGroup = screen.getByRole("radiogroup", {
      name: "adk code status strip",
    });
    await user.click(
      within(statusGroup).getByRole("radio", {
        name: "Visible",
      }),
    );
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() =>
      expect(hostControl.updateHostSettings).toHaveBeenCalledWith({
        ...HOST_DEFAULT,
        adk_statusline: true,
      }),
    );
  });

  it("retains edits across configuration panels without changing the default agent", async () => {
    const user = userEvent.setup();
    await renderSettings("Launch");
    await hostReady();
    await user.click(screen.getByRole("button", { name: "Codex" }));
    await user.click(screen.getByRole("radio", { name: "Full access" }));
    await user.click(screen.getByRole("button", { name: "OpenCode & adk" }));
    await user.click(screen.getByRole("radio", { name: "Auto-approve" }));
    await user.click(screen.getByRole("button", { name: "Codex" }));
    expect(screen.getByRole("radio", { name: "Full access" })).toBeChecked();
    expect(
      within(
        screen.getByRole("radiogroup", { name: "Default agent" }),
      ).getByRole("radio", { name: "Claude" }),
    ).toBeChecked();
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(hostControl.updateHostSettings).toHaveBeenCalledWith({
        ...HOST_DEFAULT,
        codex_sandbox_mode: "danger-full-access",
        opencode_auto: true,
      }),
    );
    expect(api.setUserSettings).toHaveBeenCalledWith(
      ["/Users/dev/mono"],
      "claude",
      DEFAULT_COLORS,
    );
  });

  it("without an authorized host the launch controls are locked, say why, and nothing falls back to the server", async () => {
    vi.mocked(hostControl.getHostSettings).mockRejectedValue(
      new Error(NOT_APPROVED_MESSAGE),
    );
    const user = userEvent.setup();
    await renderSettings("Launch");
    await screen.findByText("/Users/dev/mono");
    expect(await screen.findByRole("alert")).toHaveTextContent(/No machine has approved this browser/);
    expect(
      screen.getByRole("radio", { name: "Use Claude settings" }),
    ).toBeDisabled();
    expect(screen.getByRole("radio", { name: "adk code" })).toBeDisabled();
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(api.setUserSettings).toHaveBeenCalled());
    expect(hostControl.updateHostSettings).not.toHaveBeenCalled();
  });

  it("shows the host's refusal when the key may not change settings", async () => {
    vi.mocked(hostControl.updateHostSettings).mockRejectedValue(
      new Error(
        "The host refused this: capability outside credential ceiling.",
      ),
    );
    const user = userEvent.setup();
    await renderSettings("Launch");
    await screen.findByText("/Users/dev/mono");
    await hostReady();
    await user.click(
      screen.getByRole("radio", {
        name: "Skip permission prompts",
      }),
    );
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(
      await screen.findByText(/Could not save settings: The host refused this/),
    ).toBeInTheDocument();
  });

  it("saves the terminal font size in this browser, not the account", async () => {
    const user = userEvent.setup();
    await renderSettings("Appearance");
    await screen.findByText("/Users/dev/mono");

    const input = screen.getByLabelText("Terminal font size");
    await user.type(input, "15{Enter}");
    expect(localStorage.getItem("arrakisTerminalFontSize")).toBe("15");
    expect(api.setUserSettings).not.toHaveBeenCalled();

    await user.click(
      screen.getByRole("button", {
        name: "Reset terminal font size to default",
      }),
    );
    expect(localStorage.getItem("arrakisTerminalFontSize")).toBeNull();
    expect(input).toHaveValue(null);
  });

  it("saves a chosen inline code colour", async () => {
    const user = userEvent.setup();
    await renderSettings("Appearance");
    await screen.findByText("/Users/dev/mono");

    // Native colour inputs don't accept user.type; set the value + fire change.
    const picker = screen.getByLabelText("Inline code colour");
    fireEvent.input(picker, { target: { value: "#123456" } });
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() =>
      expect(api.setUserSettings).toHaveBeenCalledWith(
        ["/Users/dev/mono"],
        "claude",
        {
          palette: [],
          rgb: [0x123456],
        },
      ),
    );
  });
});

describe("Settings — user management", () => {
  beforeEach(() => {
    vi.mocked(api.authConfig).mockResolvedValue({
      google_enabled: false,
      password_enabled: true,
      user: {
        email: "admin@example.com",
        display_name: "Admin",
        is_admin: true,
      },
    });
    vi.mocked(api.listAdminUsers).mockResolvedValue([
      {
        email: "admin@example.com",
        display_name: "Initial Admin",
        is_admin: true,
        is_initial: true,
      },
      {
        email: "viewer@example.com",
        display_name: "Delegated Admin",
        is_admin: true,
        is_initial: false,
      },
      {
        email: "member@example.com",
        display_name: "Romain",
        is_admin: false,
        is_initial: false,
      },
    ]);
  });

  it("lists every user with the effective administrator status", async () => {
    await renderSettings("Administration");

    const initialRow = (await screen.findByText("admin@example.com")).closest(
      "li",
    )!;
    const delegatedRow = screen.getByText("viewer@example.com").closest("li")!;
    const userRow = screen.getByText("member@example.com").closest("li")!;

    expect(within(initialRow).getByText("Initial administrator")).toBeVisible();
    expect(within(delegatedRow).getByText("Administrator")).toBeVisible();
    expect(within(userRow).getByText("User")).toBeVisible();
    expect(
      within(initialRow).queryByRole("button", {
        name: /Remove administrator/,
      }),
    ).not.toBeInTheDocument();
  });

  it("keeps role management but hides reset links without password auth", async () => {
    vi.mocked(api.authConfig).mockResolvedValue({
      google_enabled: true,
      password_enabled: false,
      user: {
        email: "admin@example.com",
        display_name: "Admin",
        is_admin: true,
      },
    });

    await renderSettings("Administration");
    const memberRow = (await screen.findByText("member@example.com")).closest(
      "li",
    )!;

    expect(
      within(memberRow).queryByRole("button", { name: /Create reset link/ }),
    ).not.toBeInTheDocument();
    expect(
      within(memberRow).getByRole("button", {
        name: "Make member@example.com an administrator",
      }),
    ).toBeVisible();
    expect(api.listAdminUsers).toHaveBeenCalledOnce();
  });

  it("creates a one-time reset link from the selected user row", async () => {
    vi.mocked(api.createPasswordResetLink).mockResolvedValue({
      reset_path: "/reset-password?token=token-value",
      expires_at: "2026-08-20T12:30:00Z",
    });

    const user = userEvent.setup();
    await renderSettings("Administration");
    const memberRow = (await screen.findByText("member@example.com")).closest(
      "li",
    )!;
    await user.click(
      within(memberRow).getByRole("button", {
        name: "Create reset link for member@example.com",
      }),
    );

    await waitFor(() =>
      expect(api.createPasswordResetLink).toHaveBeenCalledWith(
        "member@example.com",
      ),
    );
    expect(
      await within(memberRow).findByLabelText(
        "One-time reset link for member@example.com",
      ),
    ).toHaveValue("http://localhost:3000/reset-password?token=token-value");
  });

  it("deletes a user only after confirming, and hides the action for initial admins and the current user", async () => {
    vi.mocked(api.deleteUser).mockResolvedValue();

    const user = userEvent.setup();
    await renderSettings("Administration");

    const initialRow = (await screen.findByText("admin@example.com")).closest(
      "li",
    )!;
    const memberRow = screen.getByText("member@example.com").closest("li")!;

    // Initial administrators and the signed-in user have no Delete button.
    expect(
      within(initialRow).queryByRole("button", { name: /^Delete / }),
    ).not.toBeInTheDocument();

    await user.click(
      within(memberRow).getByRole("button", {
        name: "Delete member@example.com",
      }),
    );

    // Confirmation dialog explains the consequences before calling the API.
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText(/Delete user\?/i)).toBeInTheDocument();
    expect(
      within(dialog).getByText(/kept as-is with their email preserved/i),
    ).toBeInTheDocument();
    expect(api.deleteUser).not.toHaveBeenCalled();

    // Cancel bails out without calling the API.
    await user.click(within(dialog).getByRole("button", { name: /Cancel/ }));
    expect(api.deleteUser).not.toHaveBeenCalled();

    // Confirming calls the API with the chosen email.
    await user.click(
      within(memberRow).getByRole("button", {
        name: "Delete member@example.com",
      }),
    );
    const dialog2 = await screen.findByRole("dialog");
    await user.click(
      within(dialog2).getByRole("button", {
        name: "Confirm deleting member@example.com",
      }),
    );
    await waitFor(() =>
      expect(api.deleteUser).toHaveBeenCalledWith("member@example.com"),
    );
  });

  it("grants and removes administrator access from user rows", async () => {
    vi.mocked(api.grantAdmin).mockResolvedValue({
      email: "member@example.com",
      is_initial: false,
    });
    vi.mocked(api.revokeAdmin).mockResolvedValue();

    const user = userEvent.setup();
    await renderSettings("Administration");
    const memberRow = (await screen.findByText("member@example.com")).closest(
      "li",
    )!;
    await user.click(
      within(memberRow).getByRole("button", {
        name: "Make member@example.com an administrator",
      }),
    );

    await waitFor(() =>
      expect(api.grantAdmin).toHaveBeenCalledWith("member@example.com"),
    );
    const delegatedRow = screen.getByText("viewer@example.com").closest("li")!;
    await user.click(
      within(delegatedRow).getByRole("button", {
        name: "Remove administrator access from viewer@example.com",
      }),
    );
    await waitFor(() =>
      expect(api.revokeAdmin).toHaveBeenCalledWith("viewer@example.com"),
    );
  });
});

describe("host network settings", () => {
  it("shows the active paired host's network settings, and none without a host", async () => {
    const { unmount } = renderWithProviders(<Settings />);
    await screen.findByRole("heading", { name: "Settings" });
    expect(screen.queryByRole("heading", { name: "Network" })).toBeNull();
    unmount();

    const host = await makeTestHost();
    await pairHost(host.descriptor, "laptop");
    try {
      renderWithProviders(<Settings />);
      await userEvent.click(
        screen.getByText("Advanced", { selector: "summary" }),
      );
      expect(
        await screen.findByRole("heading", { name: "Network" }),
      ).toBeInTheDocument();
    } finally {
      unpairHost(host.id);
    }
  });
});

describe("Settings on the local host", () => {
  it("keeps the same preference controls and saves locally without hosted account requests", async () => {
    vi.mocked(useLocalHost).mockReturnValue({
      local: true,
      authenticated: true,
      error: null,
    });
    vi.mocked(localRequest).mockImplementation(async (path, init) => {
      if (path === "/api/local/preferences")
        return (
          init?.method === "PUT"
            ? JSON.parse(String(init.body))
            : { container_paths: ["/local/repo"], default_agent: "claude" }
        ) as never;
      if (path === "/api/status")
        return {
          descriptor: "host-descriptor",
          server: "https://hosted.example",
          credentials: [],
        } as never;
      if (path.startsWith("/api/users?")) return { users: [] } as never;
      throw new Error("Unexpected local endpoint");
    });
    const user = userEvent.setup();
    renderWithProviders(<Settings />);
    expect(
      screen.getByRole("heading", { name: "Access to this host" }),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Launch" }));
    await hostReady();
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(localRequest).toHaveBeenCalledWith(
        "/api/local/preferences",
        expect.objectContaining({ method: "PUT" }),
      ),
    );
    const call = vi
      .mocked(localRequest)
      .mock.calls.find(
        ([path, init]) =>
          path === "/api/local/preferences" && init?.method === "PUT",
      )!;
    expect(JSON.parse(String(call[1]?.body))).toEqual({
      container_paths: ["/local/repo"],
      default_agent: "claude",
      inline_code_colors: DEFAULT_COLORS,
    });
    expect(api.authConfig).not.toHaveBeenCalled();
    expect(api.getUserSettings).not.toHaveBeenCalled();
    expect(api.setUserSettings).not.toHaveBeenCalled();
    expect(api.userKeyDirectory).not.toHaveBeenCalled();
    expect(api.listAdminUsers).not.toHaveBeenCalled();
  });
});

describe("Settings sections", () => {
  it("opens Host access and shows only the selected section", async () => {
    const user = userEvent.setup();
    renderWithProviders(<Settings />);
    expect(
      await screen.findByRole("textbox", { name: "Browser name" }),
    ).toBeVisible();
    expect(screen.getByRole("button", { name: "Host access" })).toHaveAttribute(
      "aria-current",
      "page",
    );
    expect(
      screen.queryByRole("radiogroup", { name: "Default agent" }),
    ).toBeNull();
    expect(screen.queryByRole("button", { name: "Administration" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
    await user.click(screen.getByRole("button", { name: "Appearance" }));
    expect(screen.getByLabelText("Inline code colour")).toBeVisible();
    expect(screen.queryByRole("textbox", { name: "Browser name" })).toBeNull();
    expect(
      screen.queryByRole("heading", { name: "Top-level repositories" }),
    ).toBeNull();
  });

  it("does not fetch remote Launch settings until the Launch section opens", async () => {
    const user = userEvent.setup();
    renderWithProviders(<Settings />);
    await screen.findByRole("textbox", { name: "Browser name" });
    await waitFor(() => expect(api.getUserSettings).toHaveBeenCalled());
    expect(hostControl.getHostSettings).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Appearance" }));
    expect(hostControl.getHostSettings).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Launch" }));
    await waitFor(() =>
      expect(hostControl.getHostSettings).toHaveBeenCalledTimes(1),
    );
  });

  it("keeps browser names and unsaved preferences when switching sections", async () => {
    const user = userEvent.setup();
    renderWithProviders(<Settings />);
    const name = await screen.findByRole("textbox", { name: "Browser name" });
    await user.clear(name);
    await user.type(name, "Work browser");
    await user.click(screen.getByRole("button", { name: "Repositories" }));
    await user.type(
      screen.getByLabelText("New container repository path"),
      "/work/new",
    );
    await user.click(screen.getByRole("button", { name: "Add" }));
    await user.click(screen.getByRole("button", { name: "Appearance" }));
    fireEvent.input(screen.getByLabelText("Inline code colour"), {
      target: { value: "#123456" },
    });
    await user.click(screen.getByRole("button", { name: "Launch" }));
    await hostReady();
    await user.click(
      within(
        screen.getByRole("radiogroup", { name: "Default agent" }),
      ).getByRole("radio", { name: "Codex" }),
    );
    await user.click(screen.getByRole("button", { name: "Host access" }));
    expect(screen.getByRole("textbox", { name: "Browser name" })).toHaveValue(
      "Work browser",
    );
    await user.click(screen.getByRole("button", { name: "Repositories" }));
    expect(screen.getByText("/work/new")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(api.setUserSettings).toHaveBeenCalledWith(
        ["/Users/dev/mono", "/work/new"],
        "codex",
        { palette: [], rgb: [0x123456] },
      ),
    );
  });
});

it("keeps device invitations and advanced host controls behind Advanced", async () => {
  const user = userEvent.setup();
  renderWithProviders(<Settings />);
  expect(
    await screen.findByRole("button", { name: "Connect machine" }),
  ).toBeVisible();
  expect(
    screen.queryByRole("button", { name: "Connect this device" }),
  ).toBeNull();
  expect(screen.queryByRole("button", { name: "Pair host" })).toBeNull();
  await user.click(screen.getByText("Advanced", { selector: "summary" }));
  expect(
    await screen.findByRole("button", { name: "Connect this device" }),
  ).toBeVisible();
  expect(screen.getByRole("button", { name: "Pair host" })).toBeVisible();
});

describe("launch settings stay on their selected machine", () => {
  it("locks another machine's pending or failed settings, then loads its own flags", async () => {
    const first = await makeTestHost();
    const second = await makeTestHost();
    await pairHost(first.descriptor, "First machine", "browser-key");
    await pairHost(second.descriptor, "Second machine", "browser-key");
    selectHost(first.id);
    let rejectSecond!: (error: Error) => void;
    vi.mocked(hostControl.getHostSettings).mockImplementation((id) =>
      id === first.id
        ? Promise.resolve({ ...HOST_DEFAULT, claude_skip_permissions: true })
        : new Promise((_resolve, reject) => {
            rejectSecond = reject;
          }),
    );
    const user = userEvent.setup();
    try {
      const { queryClient } = await renderSettings("Launch");
      await waitFor(() =>
        expect(
          screen.getByRole("radio", { name: "Skip permission prompts" }),
        ).toBeChecked(),
      );
      await user.click(screen.getByRole("button", { name: "Host access" }));
      await user.selectOptions(
        screen.getByRole("combobox", { name: "Machine" }),
        second.id,
      );
      await user.click(screen.getByRole("button", { name: "Launch" }));
      expect(
        screen.getByRole("radio", { name: "Skip permission prompts" }),
      ).toBeDisabled();
      await act(async () => {
        rejectSecond(new Error("Second machine settings unavailable"));
      });
      expect(
        await screen.findByText("Second machine settings unavailable"),
      ).toBeVisible();
      expect(
        screen.getByRole("radio", { name: "Use Claude settings" }),
      ).toBeDisabled();
      await user.click(screen.getByRole("button", { name: "Save" }));
      await screen.findByText("Settings saved");
      expect(hostControl.updateHostSettings).not.toHaveBeenCalled();
      vi.mocked(hostControl.getHostSettings).mockResolvedValue({
        ...HOST_DEFAULT,
      });
      await user.click(screen.getByRole("button", { name: "Try again" }));
      await waitFor(() =>
        expect(
          screen.getByRole("radio", { name: "Use Claude settings" }),
        ).toBeEnabled(),
      );
      expect(
        screen.getByRole("radio", { name: "Use Claude settings" }),
      ).toBeChecked();
      expect(
        queryClient.getQueryData([
          "host-launch-settings",
          first.id,
          "browser-key",
        ]),
      ).toEqual({ ...HOST_DEFAULT, claude_skip_permissions: true });
      vi.mocked(hostControl.getHostSettings).mockRejectedValue(
        new Error("Passkey authorization required"),
      );
      await act(async () => {
        setHostAuthMode(second.id, "passkey");
      });
      expect(
        await screen.findByText("Passkey authorization required"),
      ).toBeVisible();
      expect(
        screen.getByRole("radio", { name: "Use Claude settings" }),
      ).toBeDisabled();
    } finally {
      await act(async () => {
        unpairHost(first.id);
        unpairHost(second.id);
      });
    }
  });

  it("pins a pending save and its cache update to the original machine", async () => {
    const first = await makeTestHost();
    const second = await makeTestHost();
    await pairHost(first.descriptor, "First machine", "browser-key");
    await pairHost(second.descriptor, "Second machine", "browser-key");
    selectHost(first.id);
    let completePreferences!: (settings: {
      container_paths: string[];
      default_agent: "claude";
      inline_code_colors: typeof DEFAULT_COLORS;
    }) => void;
    vi.mocked(api.setUserSettings).mockImplementation(
      () =>
        new Promise((resolve) => {
          completePreferences = resolve;
        }),
    );
    const user = userEvent.setup();
    try {
      const { queryClient } = await renderSettings("Launch");
      await hostReady();
      await user.click(
        screen.getByRole("radio", { name: "Skip permission prompts" }),
      );
      await user.click(screen.getByRole("button", { name: "Save" }));
      expect(hostControl.updateHostSettings).not.toHaveBeenCalled();
      await user.click(screen.getByRole("button", { name: "Host access" }));
      await user.selectOptions(
        screen.getByRole("combobox", { name: "Machine" }),
        second.id,
      );
      await user.click(screen.getByRole("button", { name: "Launch" }));
      await hostReady();
      expect(
        screen.getByRole("radio", { name: "Use Claude settings" }),
      ).toBeChecked();
      await act(async () => {
        completePreferences({
          container_paths: ["/Users/dev/mono"],
          default_agent: "claude",
          inline_code_colors: DEFAULT_COLORS,
        });
      });
      await waitFor(() =>
        expect(hostControl.updateHostSettings).toHaveBeenCalledWith(
          { ...HOST_DEFAULT, claude_skip_permissions: true },
          first.id,
        ),
      );
      expect(hostControl.updateHostSettings).toHaveBeenCalledTimes(1);
      expect(
        queryClient.getQueryData([
          "host-launch-settings",
          first.id,
          "browser-key",
        ]),
      ).toEqual({ ...HOST_DEFAULT, claude_skip_permissions: true });
      expect(
        queryClient.getQueryData([
          "host-launch-settings",
          second.id,
          "browser-key",
        ]),
      ).toEqual(HOST_DEFAULT);
      expect(
        screen.getByRole("radio", { name: "Use Claude settings" }),
      ).toBeChecked();
    } finally {
      await act(async () => {
        unpairHost(first.id);
        unpairHost(second.id);
      });
    }
  });
});
