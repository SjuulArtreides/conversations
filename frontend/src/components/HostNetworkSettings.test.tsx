import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { HostNetworkSettings } from "./HostNetworkSettings";
import { hostRtcConfig } from "@/lib/hostNetwork";
import { pairHost } from "@/lib/hostPairing";
import fixture from "@/lib/fixtures/host_statements.json";

const HOST = fixture.host.id;
const SECRET = "turn-secret-Zq81";

beforeEach(async () => {
  localStorage.clear();
  vi.restoreAllMocks();
  await pairHost(fixture.host.descriptor, "laptop");
});

describe("HostNetworkSettings", () => {
  it("renders nothing for a host that is not paired", () => {
    renderWithProviders(<HostNetworkSettings hostId={"A".repeat(43)} />);
    expect(screen.queryByRole("region")).not.toBeInTheDocument();
    expect(screen.queryByRole("button")).not.toBeInTheDocument();
  });

  it("says that empty means direct connections only and stays in this browser", () => {
    renderWithProviders(<HostNetworkSettings hostId={HOST} />);
    const section = screen.getByRole("region", { name: /network/i });
    expect(section).toHaveTextContent(/direct connections only/i);
    expect(section).toHaveTextContent(/only in this browser/i);
    expect(screen.getByTestId("network-mode")).toHaveTextContent(/direct/i);
  });

  it("saves a forced-TURN server the RTC connection then uses, without calling the server", async () => {
    const fetchSpy = vi.spyOn(globalThis, "fetch");
    const user = userEvent.setup();
    renderWithProviders(<HostNetworkSettings hostId={HOST} />);
    await user.click(screen.getByRole("button", { name: /add server/i }));
    const row = screen.getByTestId("ice-server-0");
    await user.type(
      within(row).getByLabelText(/urls/i),
      "turn:127.0.0.1:3478?transport=udp",
    );
    await user.type(within(row).getByLabelText(/username/i), "alice");
    await user.type(within(row).getByLabelText(/credential/i), SECRET);
    expect(within(row).getByLabelText(/credential/i)).toHaveAttribute(
      "type",
      "password",
    );
    await user.click(screen.getByLabelText(/relay only/i));
    await user.click(screen.getByRole("button", { name: /save network/i }));

    expect(await screen.findByTestId("network-mode")).toHaveTextContent(
      /relay only/i,
    );
    expect(hostRtcConfig(HOST)).toMatchObject({
      iceServers: [
        {
          urls: "turn:127.0.0.1:3478?transport=udp",
          username: "alice",
          credential: SECRET,
        },
      ],
      iceTransportPolicy: "relay",
    });
    expect(fetchSpy).not.toHaveBeenCalled();
    expect(document.body.textContent).not.toContain(SECRET);
  });

  it("keeps several URLs of one server as a list", async () => {
    const user = userEvent.setup();
    renderWithProviders(<HostNetworkSettings hostId={HOST} />);
    await user.click(screen.getByRole("button", { name: /add server/i }));
    await user.type(
      within(screen.getByTestId("ice-server-0")).getByLabelText(/urls/i),
      "stun:stun.example.com{Enter}stun:192.0.2.1:3478",
    );
    await user.click(screen.getByRole("button", { name: /save network/i }));
    expect(hostRtcConfig(HOST).iceServers).toEqual([
      { urls: ["stun:stun.example.com", "stun:192.0.2.1:3478"] },
    ]);
  });

  it("shows a bounded error and keeps the old settings when input is refused", async () => {
    const user = userEvent.setup();
    renderWithProviders(<HostNetworkSettings hostId={HOST} />);
    const before = hostRtcConfig(HOST);
    await user.click(screen.getByRole("button", { name: /add server/i }));
    await user.type(
      within(screen.getByTestId("ice-server-0")).getByLabelText(/urls/i),
      "turn:turn.example.com",
    );
    await user.type(
      within(screen.getByTestId("ice-server-0")).getByLabelText(/credential/i),
      SECRET,
    );
    await user.click(screen.getByRole("button", { name: /save network/i }));
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent(/needs a username and credential/i);
    expect(alert).not.toHaveTextContent(SECRET);
    expect(hostRtcConfig(HOST)).toEqual(before);
  });

  it("reports corrupt saved settings and can clear them back to direct", async () => {
    localStorage.setItem("conversations.hostNetwork", "not json");
    const user = userEvent.setup();
    renderWithProviders(<HostNetworkSettings hostId={HOST} />);
    expect(screen.getByRole("alert")).toHaveTextContent(/invalid; fix or clear/i);
    await user.click(screen.getByRole("button", { name: /clear/i }));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(hostRtcConfig(HOST)).toMatchObject({
      iceServers: [],
      iceTransportPolicy: "all",
    });
  });

  it("loads saved settings with the credential masked and removes a server", async () => {
    const user = userEvent.setup();
    const first = renderWithProviders(<HostNetworkSettings hostId={HOST} />);
    await user.click(screen.getByRole("button", { name: /add server/i }));
    await user.type(
      within(screen.getByTestId("ice-server-0")).getByLabelText(/urls/i),
      "turn:127.0.0.1:3478",
    );
    await user.type(
      within(screen.getByTestId("ice-server-0")).getByLabelText(/username/i),
      "alice",
    );
    await user.type(
      within(screen.getByTestId("ice-server-0")).getByLabelText(/credential/i),
      SECRET,
    );
    await user.click(screen.getByRole("button", { name: /save network/i }));
    first.unmount();

    renderWithProviders(<HostNetworkSettings hostId={HOST} />);
    const row = screen.getByTestId("ice-server-0");
    expect(within(row).getByLabelText(/urls/i)).toHaveValue("turn:127.0.0.1:3478");
    expect(within(row).getByLabelText(/credential/i)).toHaveAttribute("type", "password");
    await user.click(within(row).getByRole("button", { name: /remove/i }));
    await user.click(screen.getByRole("button", { name: /save network/i }));
    expect(hostRtcConfig(HOST).iceServers).toEqual([]);
    expect(screen.getByTestId("network-mode")).toHaveTextContent(/direct/i);
  });
});
