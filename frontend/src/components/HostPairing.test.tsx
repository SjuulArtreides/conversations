import "fake-indexeddb/auto";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { HostPairing } from "./HostPairing";
import { activeHost, unpairHost } from "@/lib/hostPairing";
import fixture from "@/lib/fixtures/host_statements.json";

const hostControl = vi.hoisted(() => ({
  hostOnline: vi.fn(),
  unlockAgain: vi.fn(),
  closePrivateChannels: vi.fn(),
}));
vi.mock("@/lib/hostControl", async () => {
  const actual =
    await vi.importActual<typeof import("@/lib/hostControl")>(
      "@/lib/hostControl",
    );
  return {
    ...actual,
    ...hostControl,
    hostControl: {
      ...actual.hostControl,
      listAccess: vi.fn().mockResolvedValue([]),
    },
  };
});

const HOST = fixture.host.id;
const DESCRIPTOR = fixture.host.descriptor;

beforeEach(() => {
  vi.clearAllMocks();
  indexedDB.deleteDatabase("conversations-host-auth");
  const active = activeHost();
  if (active) unpairHost(active.id);
  hostControl.hostOnline.mockResolvedValue(false);
});

describe("HostPairing", () => {
  it("says plainly that nothing can be started until a host is paired", () => {
    renderWithProviders(<HostPairing />);
    expect(screen.getByTestId("no-hosts")).toHaveTextContent(
      /starting and reopening sessions is off until you pair one/i,
    );
    expect(
      screen.getByText(/this page cannot approve a passkey/i),
    ).toBeInTheDocument();
  });

  it("rejects a malformed descriptor and pins the key and id of a valid one", async () => {
    const user = userEvent.setup();
    renderWithProviders(<HostPairing />);
    await user.type(screen.getByLabelText(/host descriptor/i), "not-a-host");
    await user.click(screen.getByRole("button", { name: "Pair host" }));
    expect(
      await screen.findByText(/not a host descriptor/i),
    ).toBeInTheDocument();
    expect(screen.queryByTestId("host-id")).not.toBeInTheDocument();

    await user.clear(screen.getByLabelText(/host descriptor/i));
    await user.type(screen.getByLabelText(/host descriptor/i), DESCRIPTOR);
    await user.type(screen.getByLabelText(/name/i), "laptop");
    await user.click(screen.getByRole("button", { name: "Pair host" }));
    expect(await screen.findByTestId("host-id")).toHaveTextContent(HOST);
    expect(screen.getByTestId("host-lock")).toHaveTextContent(/locked/i);
    expect(screen.getByTestId("host-online")).toHaveTextContent("Host offline");
  });

  it("unlocks through the passkey flow and can forget the host", async () => {
    const user = userEvent.setup();
    hostControl.hostOnline.mockResolvedValue(true);
    hostControl.unlockAgain.mockResolvedValue({});
    renderWithProviders(<HostPairing />);
    await user.type(screen.getByLabelText(/host descriptor/i), DESCRIPTOR);
    await user.click(screen.getByRole("button", { name: "Pair host" }));
    await user.click(
      await screen.findByRole("button", { name: "Unlock with passkey" }),
    );
    await waitFor(() =>
      expect(hostControl.unlockAgain).toHaveBeenCalledWith(HOST),
    );
    await waitFor(() =>
      expect(screen.getByTestId("host-online")).toHaveTextContent(
        "Host online",
      ),
    );
    await user.click(screen.getByRole("button", { name: "Forget host" }));
    expect(await screen.findByTestId("no-hosts")).toBeInTheDocument();
  });

  it("asks to re-pair an entry saved without a pinned key and will not unlock it", async () => {
    localStorage.setItem(
      "conversations.hosts",
      JSON.stringify({ hosts: [{ id: HOST, label: "old" }], active: HOST }),
    );
    renderWithProviders(<HostPairing />);
    expect(await screen.findByTestId("host-repair")).toHaveTextContent(
      /pair again/i,
    );
    expect(
      screen.getByRole("button", { name: "Unlock with passkey" }),
    ).toBeDisabled();
  });
});

it("lets an owner-approved browser key use direct mode while retaining the host pin", async () => {
  const user = userEvent.setup();
  renderWithProviders(<HostPairing />);
  await user.type(screen.getByLabelText(/host descriptor/i), DESCRIPTOR);
  await user.selectOptions(
    screen.getByLabelText("Authorization"),
    "browser-key",
  );
  await user.click(screen.getByRole("button", { name: "Pair host" }));
  await waitFor(() => expect(activeHost()?.authMode).toBe("browser-key"));
  expect(activeHost()?.id).toBe(HOST);
  expect(activeHost()?.key).toBe(DESCRIPTOR.slice("chd1.".length));
  expect(
    screen.queryByRole("button", { name: "Unlock with passkey" }),
  ).not.toBeInTheDocument();
  expect(
    screen.getByText(/host must have approved this browser key locally/),
  ).toBeInTheDocument();
});
