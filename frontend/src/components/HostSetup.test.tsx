import "fake-indexeddb/auto";
import * as React from "react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { HostSetup } from "./HostSetup";
import {
  activeHost,
  pairHost,
  selectHost,
  setHostAuthMode,
} from "@/lib/hostPairing";
import { HostControlError } from "@/lib/hostControl";
import { makeTestHost } from "@/lib/testHost";
import fixture from "@/lib/fixtures/host_statements.json";

const mocks = vi.hoisted(() => ({
  online: vi.fn(),
  access: vi.fn(),
  publish: vi.fn(),
  unlock: vi.fn(),
  registration: false,
}));
vi.mock("@/lib/hostSetup", () => ({ inspectHostAccess: mocks.access }));
vi.mock("@/lib/hostControl", async () => ({
  ...(await vi.importActual<typeof import("@/lib/hostControl")>(
    "@/lib/hostControl",
  )),
  hostOnline: mocks.online,
  unlockAgain: mocks.unlock,
  closePrivateChannels: vi.fn(),
}));
vi.mock("@/lib/browserPublication", async () => {
  const { useQuery, useMutation, useQueryClient } =
    await import("@tanstack/react-query");
  return {
    useBrowserPublication: () => {
      const client = useQueryClient();
      return {
        key: useQuery({
          queryKey: ["test-browser-key"],
          queryFn: async () => ({
            kid: "browser-fingerprint",
            publicRaw: new Uint8Array([4, 1, 2, 3]),
          }),
        }),
        registration: useQuery({
          queryKey: ["test-browser-registration"],
          queryFn: async () => mocks.registration,
        }),
        publish: useMutation({
          mutationFn: (label: string) => mocks.publish(label),
          retry: false,
          onSuccess: () => {
            client.setQueryData(["test-browser-registration"], true);
          },
        }),
      };
    },
  };
});

const originalInterval = globalThis.setInterval;
beforeEach(() => {
  vi.resetAllMocks();
  localStorage.removeItem("conversations.hosts");
  mocks.registration = false;
  mocks.online.mockResolvedValue(true);
  mocks.access.mockRejectedValue(
    new HostControlError("Browser key not approved", "refused"),
  );
  mocks.publish.mockResolvedValue(undefined);
  mocks.unlock.mockResolvedValue(undefined);
  // Exercise the production polling path without waiting five seconds per transition.
  vi.spyOn(globalThis, "setInterval").mockImplementation(
    (handler, timeout, ...args) =>
      originalInterval(handler, timeout === 5000 ? 30 : timeout, ...args),
  );
});
afterEach(() => vi.restoreAllMocks());
async function connect(mode: "browser-key" | "passkey" = "browser-key") {
  await pairHost(fixture.host.descriptor, "Coder", mode);
}
function progress() {
  return within(screen.getByRole("list", { name: "Host setup progress" }));
}

it("connects a public machine descriptor in browser-key mode and keeps browser name editable", async () => {
  const user = userEvent.setup();
  renderWithProviders(<HostSetup />);
  expect(progress().getByText("Not connected")).toBeVisible();
  expect(mocks.online).not.toHaveBeenCalled();
  await user.type(
    screen.getByRole("textbox", { name: "Browser name" }),
    " Work",
  );
  await user.type(
    screen.getByRole("textbox", { name: "Host descriptor" }),
    fixture.host.descriptor,
  );
  await user.type(
    screen.getByRole("textbox", { name: "Machine name" }),
    "Coder",
  );
  await user.click(screen.getByRole("button", { name: "Connect machine" }));
  expect(
    await screen.findByRole("button", {
      name: "Prepare this browser for approval",
    }),
  ).toBeVisible();
  expect(activeHost()?.authMode).toBe("browser-key");
  expect(screen.getByRole("textbox", { name: "Browser name" })).toHaveValue(
    "My browser Work",
  );
  expect(mocks.publish).not.toHaveBeenCalled();
  expect(mocks.unlock).not.toHaveBeenCalled();
});

it("offers a latest CLI download link only when no machine is paired", async () => {
  const { unmount } = renderWithProviders(<HostSetup />);
  const link = screen.getByRole("link", { name: "Download Conversations" });
  expect(link).toHaveAttribute(
    "href",
    "https://github.com/SjuulArtreides/conversations/releases/latest",
  );
  expect(link).toHaveAttribute("rel", "noopener noreferrer");
  expect(
    screen.getByRole("button", { name: "Install with Bash" }),
  ).toBeVisible();
  expect(
    screen.getByRole("link", { name: "build from source" }),
  ).toHaveAttribute(
    "href",
    "https://github.com/SjuulArtreides/conversations#run-from-source",
  );
  unmount();

  mocks.online.mockResolvedValue(false);
  await connect();
  renderWithProviders(<HostSetup />);
  await screen.findByText("Offline");
  expect(
    screen.queryByRole("link", { name: "Download Conversations" }),
  ).toBeNull();
  expect(
    screen.queryByRole("button", { name: "Install with Bash" }),
  ).toBeNull();
});

it("walks a new user from install through start, descriptor, connect and approval", () => {
  renderWithProviders(<HostSetup />);
  const steps = within(
    screen.getByRole("list", { name: "Connect a machine" }),
  ).getAllByRole("listitem");
  expect(steps.map((step) => step.querySelector("p")?.textContent)).toEqual([
    "Install the conversations CLI on that machine",
    "Log in to this server and start the host",
    "Copy the public host descriptor",
    "Paste it here and connect",
    "Prepare and approve this browser",
  ]);
  expect(steps[1]).toHaveTextContent(
    `conversations login --server '${window.location.origin}'`,
  );
  expect(steps[1]).toHaveTextContent(
    `conversations host --server '${window.location.origin}'`,
  );
  expect(steps[2]).toHaveTextContent(
    "conversations credential --runtime-dir … descriptor",
  );
  expect(
    within(steps[3]).getByRole("textbox", { name: "Host descriptor" }),
  ).toBeVisible();
  expect(
    within(steps[3]).getByRole("button", { name: "Connect machine" }),
  ).toBeDisabled();
});

it("leads waiting approval with the next action and terminal approval for remote machines", async () => {
  await connect();
  mocks.registration = true;
  renderWithProviders(<HostSetup />);
  expect(
    await screen.findByText("Next: approve this browser on Coder."),
  ).toBeVisible();
  const terminal = screen.getByRole("region", {
    name: "Terminal approval instructions",
  });
  expect(
    within(terminal).getByText(/^conversations credential add/),
  ).toBeVisible();
  expect(
    within(terminal).getByRole("button", { name: "Copy approval command" }),
  ).toBeVisible();
  const local = screen.getByRole("region", {
    name: "Or approve on the local host page",
  });
  expect(local).toHaveTextContent("localhost");
  expect(local).toHaveTextContent("Trust public key");
  expect(screen.queryByRole("button", { name: "Go to sessions" })).toBeNull();
  expect(mocks.publish).not.toHaveBeenCalled();
});

const site = "https://conversations.example.com";
const installPrefix =
  `bash -c 'f=$(mktemp) || exit; cleanup() { rm -f -- "$f"; }; ` +
  `trap cleanup EXIT; ` +
  `curl -fsSL --proto =https https://raw.githubusercontent.com/SjuulArtreides/conversations/main/scripts/install.sh ` +
  `-o "$f" && bash "$f"`;
const installCommand = `${installPrefix} --server "$1"' bash '${site}'`;
const genericInstallCommand = `${installPrefix}'`;

it("copies the exact bash installer one-liner, passing this site's origin as a positional parameter", async () => {
  const writeText = vi
    .spyOn(navigator.clipboard, "writeText")
    .mockResolvedValue(undefined);
  const user = userEvent.setup();
  renderWithProviders(<HostSetup webOrigin={site} />);
  await user.click(screen.getByRole("button", { name: "Install with Bash" }));
  expect(writeText).toHaveBeenCalledTimes(1);
  expect(writeText).toHaveBeenCalledWith(installCommand);
  expect(installCommand).not.toMatch(/curl[^|]*\|\s*bash/);
  expect(await screen.findByRole("status")).toHaveTextContent(
    "Install command copied to the clipboard.",
  );
});

it("uses this page's origin by default", async () => {
  const writeText = vi
    .spyOn(navigator.clipboard, "writeText")
    .mockResolvedValue(undefined);
  const user = userEvent.setup();
  renderWithProviders(<HostSetup />);
  await user.click(screen.getByRole("button", { name: "Install with Bash" }));
  expect(writeText).toHaveBeenCalledWith(
    `${installPrefix} --server "$1"' bash '${window.location.origin}'`,
  );
  expect(screen.getByLabelText("Login command")).toHaveTextContent(
    `conversations login --server '${window.location.origin}'`,
  );
});

it("shows exact same-server login and host commands for manual installs", () => {
  renderWithProviders(<HostSetup webOrigin={site} />);
  expect(screen.getByLabelText("Login command").textContent).toBe(
    `conversations login --server '${site}'`,
  );
  expect(screen.getByLabelText("Host command").textContent).toBe(
    `conversations host --server '${site}'`,
  );
  expect(screen.getByLabelText("Login command")).toHaveClass("select-all");
});

it.each([
  "null",
  "https://Conversations.example.com",
  "https://conversations.example.com/app",
  "https://x.example'; touch pwned; '",
  "https://x.example$(id)",
  "file://",
])(
  "never embeds a non-canonical origin (%s) in a shell command",
  async (webOrigin) => {
    const writeText = vi
      .spyOn(navigator.clipboard, "writeText")
      .mockResolvedValue(undefined);
    const user = userEvent.setup();
    renderWithProviders(<HostSetup webOrigin={webOrigin} />);
    await user.click(
      screen.getByRole("button", { name: "Install with Bash" }),
    );
    expect(writeText).toHaveBeenCalledWith(genericInstallCommand);
    expect(screen.queryByLabelText("Login command")).toBeNull();
    expect(screen.queryByLabelText("Host command")).toBeNull();
    expect(
      screen.getByText(/conversations login --server <URL>/),
    ).toBeVisible();
  },
);

it("shows the selectable installer command when clipboard copy fails", async () => {
  vi.spyOn(navigator.clipboard, "writeText").mockRejectedValue(
    new Error("denied"),
  );
  const user = userEvent.setup();
  renderWithProviders(<HostSetup webOrigin={site} />);
  await user.click(screen.getByRole("button", { name: "Install with Bash" }));
  const fallback = await screen.findByRole("status");
  expect(fallback).toHaveTextContent("Could not copy");
  const command = within(fallback).getByText(/^bash -c /);
  expect(command.textContent).toBe(installCommand);
  expect(command).toHaveClass("select-all");
});

it("distinguishes a pending connection probe from an offline machine", async () => {
  await connect();
  let finish!: (online: boolean) => void;
  mocks.online.mockImplementation(
    () =>
      new Promise<boolean>((resolve) => {
        finish = resolve;
      }),
  );
  renderWithProviders(<HostSetup />);
  expect(progress().getByText("Checking connection…")).toBeVisible();
  expect(progress().queryByText("Offline")).toBeNull();
  await act(async () => {
    finish(false);
  });
  expect(await screen.findByText("Offline")).toBeVisible();
  expect(screen.getByRole("status")).toHaveTextContent("on Coder");
  expect(mocks.access).not.toHaveBeenCalled();
});

it("shows publication failures and retries with the edited name", async () => {
  await connect();
  mocks.publish.mockRejectedValueOnce(new Error("Publication unavailable"));
  const user = userEvent.setup();
  renderWithProviders(<HostSetup />);
  const prepare = await screen.findByRole("button", {
    name: "Prepare this browser for approval",
  });
  const name = screen.getByRole("textbox", { name: "Browser name" });
  await user.clear(name);
  await user.type(name, "Work browser");
  await user.click(prepare);
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Publication unavailable",
  );
  expect(name).toHaveValue("Work browser");
  await user.click(prepare);
  expect(
    await screen.findByText(/Waiting for browser approval on/),
  ).toHaveTextContent("Coder");
  expect(mocks.publish).toHaveBeenLastCalledWith("Work browser");
  expect(
    screen.getByText(/does not notify this machine or grant access/),
  ).toBeVisible();
  expect(screen.queryByRole("button", { name: "Go to sessions" })).toBeNull();
});

it("updates a waiting browser name explicitly and gives a copyable owner approval command", async () => {
  await connect();
  mocks.registration = true;
  const user = userEvent.setup();
  renderWithProviders(<HostSetup />);
  await screen.findByText(/Waiting for browser approval on/);
  expect(
    screen.queryByRole("button", { name: "Update browser name" }),
  ).toBeNull();
  const name = screen.getByRole("textbox", { name: "Browser name" });
  await user.clear(name);
  await user.type(name, "Work's $(echo never)");
  mocks.publish.mockRejectedValueOnce(new Error("Name update unavailable"));
  await user.click(screen.getByRole("button", { name: "Update browser name" }));
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Name update unavailable",
  );
  await user.click(screen.getByRole("button", { name: "Update browser name" }));
  expect(await screen.findByText("Browser name updated.")).toBeVisible();
  expect(mocks.publish).toHaveBeenLastCalledWith("Work's $(echo never)");
  expect(
    screen.getByRole("region", { name: "Terminal approval instructions" }),
  ).toHaveTextContent("remote machine such as Coder");
  const command = screen.getByText(
    /^conversations credential add/,
  ).textContent!;
  expect(command).toContain("--public-key BAECAw==");
  expect(command).toContain(`--label 'Work'"'"'s $(echo never)'`);
  expect(command).toContain("--grant session.write");
  expect(command).toContain("--grant passkey.manage");
  expect(screen.queryByRole("button", { name: "Go to sessions" })).toBeNull();
});

it("restores publication and detects approval by polling without Retry, invalidating once", async () => {
  await connect();
  mocks.registration = true;
  const { queryClient } = renderWithProviders(<HostSetup />);
  const invalidate = vi.spyOn(queryClient, "invalidateQueries");
  expect(
    await screen.findByText(/Waiting for browser approval on/),
  ).toBeVisible();
  mocks.access.mockResolvedValue({ canRead: true, canStart: true });
  expect(
    await screen.findByRole("button", { name: "Go to sessions" }),
  ).toBeVisible();
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(
    invalidate.mock.calls.filter(
      ([options]) =>
        JSON.stringify(options?.queryKey) ===
        JSON.stringify(["private", fixture.host.id]),
    ),
  ).toHaveLength(1);
  expect(
    invalidate.mock.calls.filter(
      ([options]) =>
        JSON.stringify(options?.queryKey) ===
        JSON.stringify(["host-launch-settings"]),
    ),
  ).toHaveLength(1);
  expect(mocks.publish).not.toHaveBeenCalled();
  expect(mocks.unlock).not.toHaveBeenCalled();
});

it("shows limited approved access without claiming readiness, then removes readiness after revocation", async () => {
  await connect();
  mocks.access.mockResolvedValue({ canRead: true, canStart: false });
  renderWithProviders(<HostSetup />);
  expect(
    await screen.findByText("Needs review — permissions are limited"),
  ).toBeVisible();
  const steps = progress().getAllByRole("listitem");
  expect(steps[1]).toHaveTextContent("✓");
  expect(screen.queryByRole("button", { name: "Go to sessions" })).toBeNull();
  mocks.access.mockResolvedValue({ canRead: true, canStart: true });
  await screen.findByRole("button", { name: "Go to sessions" });
  mocks.access.mockRejectedValue(
    new HostControlError("Credential revoked", "refused"),
  );
  await waitFor(() =>
    expect(screen.queryByRole("button", { name: "Go to sessions" })).toBeNull(),
  );
  expect(
    await screen.findByRole("button", {
      name: "Prepare this browser for approval",
    }),
  ).toBeVisible();
});

it("does not carry ready data through transport errors or host switches", async () => {
  await connect();
  mocks.access.mockResolvedValue({ canRead: true, canStart: true });
  const user = userEvent.setup();
  renderWithProviders(<HostSetup />);
  await screen.findByRole("button", { name: "Go to sessions" });
  mocks.access.mockRejectedValue(
    new HostControlError("Connection interrupted", "transport"),
  );
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Connection interrupted",
  );
  expect(screen.queryByRole("button", { name: "Go to sessions" })).toBeNull();
  const other = await makeTestHost();
  mocks.online.mockImplementation(async (id: string) => id !== other.id);
  await act(async () => {
    await pairHost(other.descriptor, "Laptop", "browser-key");
  });
  expect(await screen.findByText("Offline")).toBeVisible();
  expect(screen.getByRole("combobox", { name: "Machine" })).toHaveValue(
    other.id,
  );
  expect(screen.queryByRole("button", { name: "Go to sessions" })).toBeNull();
  await user.selectOptions(
    screen.getByRole("combobox", { name: "Machine" }),
    fixture.host.id,
  );
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Connection interrupted",
  );
});

it("preserves passkey mode, unlocks only on click and respects delegation expiry", async () => {
  await connect("passkey");
  mocks.access.mockRejectedValue(
    new HostControlError("Delegation expired", "locked"),
  );
  const user = userEvent.setup();
  renderWithProviders(<HostSetup />);
  const unlock = await screen.findByRole("button", {
    name: "Unlock with passkey",
  });
  expect(mocks.unlock).not.toHaveBeenCalled();
  expect(
    screen.queryByRole("button", { name: "Prepare this browser for approval" }),
  ).toBeNull();
  mocks.access.mockResolvedValue({ canRead: true, canStart: true });
  await user.click(unlock);
  await screen.findByRole("button", { name: "Go to sessions" });
  expect(mocks.unlock).toHaveBeenCalledOnce();
  expect(activeHost()?.authMode).toBe("passkey");
  mocks.access.mockRejectedValue(
    new HostControlError("Delegation expired", "locked"),
  );
  expect(
    await screen.findByRole("button", { name: "Unlock with passkey" }),
  ).toBeVisible();
  expect(screen.queryByRole("button", { name: "Go to sessions" })).toBeNull();
  expect(mocks.unlock).toHaveBeenCalledOnce();
});

it("polls only while the section and document are visible, then stops on unmount", async () => {
  await connect();
  const { unmount } = renderWithProviders(<HostSetup active={false} />);
  await new Promise((resolve) => setTimeout(resolve, 75));
  expect(mocks.online).not.toHaveBeenCalled();
  // Rerender within the same provider tree by using a stateful owner.
  unmount();
  let setActive!: (active: boolean) => void;
  function Owner() {
    const [active, update] = React.useState(true);
    setActive = update;
    return <HostSetup active={active} />;
  }
  const rendered = renderWithProviders(<Owner />);
  await screen.findByRole("button", {
    name: "Prepare this browser for approval",
  });
  await act(async () => {
    setActive(false);
  });
  const count = mocks.access.mock.calls.length;
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(mocks.access).toHaveBeenCalledTimes(count);
  const visibility = vi.spyOn(document, "visibilityState", "get");
  visibility.mockReturnValue("hidden");
  await act(async () => {
    document.dispatchEvent(new Event("visibilitychange"));
    setActive(true);
  });
  await new Promise((resolve) => setTimeout(resolve, 75));
  expect(mocks.access).toHaveBeenCalledTimes(count);
  visibility.mockReturnValue("visible");
  await act(async () => {
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await waitFor(() =>
    expect(mocks.access.mock.calls.length).toBeGreaterThan(count),
  );
  rendered.unmount();
  const afterUnmount = mocks.access.mock.calls.length;
  await new Promise((resolve) => setTimeout(resolve, 75));
  expect(mocks.access).toHaveBeenCalledTimes(afterUnmount);
});

it("ignores an old passkey unlock failure after switching machines", async () => {
  await connect("passkey");
  const other = await makeTestHost();
  await pairHost(other.descriptor, "Laptop", "passkey");
  selectHost(fixture.host.id);
  mocks.access.mockRejectedValue(
    new HostControlError("Unlock needed", "locked"),
  );
  let rejectUnlock!: (error: Error) => void;
  mocks.unlock.mockImplementationOnce(
    () =>
      new Promise<void>((_resolve, reject) => {
        rejectUnlock = reject;
      }),
  );
  const user = userEvent.setup();
  renderWithProviders(<HostSetup />);
  await user.click(
    await screen.findByRole("button", { name: "Unlock with passkey" }),
  );
  await user.selectOptions(
    screen.getByRole("combobox", { name: "Machine" }),
    other.id,
  );
  expect(
    await screen.findByRole("button", { name: "Unlock with passkey" }),
  ).toBeEnabled();
  await act(async () => {
    rejectUnlock(new Error("Old machine unlock failed"));
  });
  expect(screen.queryByText("Old machine unlock failed")).toBeNull();
  expect(
    screen.getByRole("button", { name: "Unlock with passkey" }),
  ).toBeEnabled();
});

it("does not carry browser approval across an authorization mode switch", async () => {
  await connect();
  mocks.access.mockResolvedValue({ canRead: true, canStart: true });
  renderWithProviders(<HostSetup />);
  await screen.findByRole("button", { name: "Go to sessions" });
  mocks.access.mockRejectedValue(
    new HostControlError("Passkey unlock needed", "locked"),
  );
  await act(async () => {
    setHostAuthMode(fixture.host.id, "passkey");
  });
  expect(
    await screen.findByRole("button", { name: "Unlock with passkey" }),
  ).toBeVisible();
  expect(screen.queryByRole("button", { name: "Go to sessions" })).toBeNull();
  expect(mocks.unlock).not.toHaveBeenCalled();
});

it("asks to repair an unpinned legacy machine without claiming a check is running", async () => {
  localStorage.setItem(
    "conversations.hosts",
    JSON.stringify({
      hosts: [{ id: fixture.host.id, label: "Old laptop" }],
      active: fixture.host.id,
    }),
  );
  renderWithProviders(<HostSetup />);
  expect(progress().getByText("Identity needs repair")).toBeVisible();
  expect(screen.getByRole("alert")).toHaveTextContent("no pinned identity key");
  expect(screen.queryByText("Checking the machine connection…")).toBeNull();
  expect(mocks.online).not.toHaveBeenCalled();
});

it.each([
  "no current grant",
  "capability outside credential ceiling",
  "operation outside the delegation's scope",
  "only an owner credential may change session metadata",
  "Unclassified refusal",
])(
  "keeps a refused metadata check in review rather than requesting reapproval: %s",
  async (message) => {
    await connect();
    mocks.registration = true;
    mocks.access.mockRejectedValue(new HostControlError(message, "refused"));
    renderWithProviders(<HostSetup />);
    expect(await screen.findByRole("alert")).toHaveTextContent(message);
    expect(screen.getByRole("alert")).toHaveTextContent("Access to this host");
    expect(
      screen.queryByRole("button", {
        name: "Prepare this browser for approval",
      }),
    ).toBeNull();
    expect(screen.queryByText(/Waiting for browser approval on/)).toBeNull();
    expect(screen.queryByRole("button", { name: "Go to sessions" })).toBeNull();
  },
);
