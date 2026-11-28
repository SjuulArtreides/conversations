import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { HostLink, parseHostLink } from "./HostLink";
import { activeHost, pairHost, unpairHost } from "@/lib/hostPairing";
import { toBase64Url } from "@/lib/hostAuth";
import { useLocalHost } from "@/lib/localHost";
import fixture from "@/lib/fixtures/host_statements.json";

vi.mock("@/lib/localHost", async (original) => ({
  ...(await original<typeof import("@/lib/localHost")>()),
  useLocalHost: vi.fn(),
}));

const control = vi.hoisted(() => ({
  finishEnrollment: vi.fn(),
  unlockHost: vi.fn(),
  closePrivateChannels: vi.fn(),
  passkeyVouch: vi.fn(),
}));
vi.mock("@/lib/hostControl", () => control);

const HOST = fixture.host.id;
const DESC = fixture.host.descriptor;
const CHALLENGE = "ab".repeat(16);
const options = { publicKey: { challenge: "AQID" } };
const hash = `#desc=${DESC}&challenge=${CHALLENGE}&options=${toBase64Url(
  new TextEncoder().encode(JSON.stringify(options)),
)}`;

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(useLocalHost).mockReturnValue({
    local: false,
    authenticated: false,
    error: null,
  });
  const active = activeHost();
  if (active) unpairHost(active.id);
  window.location.hash = "";
});

describe("parseHostLink", () => {
  it("accepts the link the host prints, deriving the id from the key, and rejects anything malformed", async () => {
    expect(await parseHostLink(hash)).toEqual({
      host: { id: HOST, key: DESC.slice(5) },
      descriptor: DESC,
      challenge: CHALLENGE,
      options,
    });
    for (const bad of [
      "",
      `#desc=short&challenge=${CHALLENGE}&options=e30`,
      `#desc=${DESC}&challenge=nothex&options=e30`,
      `#desc=${DESC}&challenge=${CHALLENGE}&options=!!!`,
    ]) {
      expect(await parseHostLink(bad)).toBeNull();
    }
  });
});

describe("HostLink", () => {
  it("keeps local owner cookies out of hosted enrollment", async () => {
    vi.mocked(useLocalHost).mockReturnValue({
      local: true,
      authenticated: true,
      error: null,
    });
    window.location.hash = hash;
    renderWithProviders(<HostLink />);
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "hosted Conversations server",
    );
    expect(
      screen.queryByRole("button", { name: "Create passkey" }),
    ).not.toBeInTheDocument();
    expect(control.finishEnrollment).not.toHaveBeenCalled();
  });

  it("explains an invalid link", async () => {
    renderWithProviders(<HostLink />);
    expect(await screen.findByTestId("host-link-invalid")).toBeInTheDocument();
  });

  it("needs the user to confirm the host, then says approval still happens on the host", async () => {
    window.location.hash = hash;
    control.finishEnrollment.mockResolvedValue("cred-id-123");
    const user = userEvent.setup();
    renderWithProviders(<HostLink />);
    expect(await screen.findByTestId("link-host-id")).toHaveTextContent(HOST);
    const button = await screen.findByRole("button", {
      name: "Create passkey",
    });
    expect(button).toBeDisabled();
    await user.click(screen.getByRole("checkbox"));
    await user.click(button);
    await waitFor(() =>
      expect(control.finishEnrollment).toHaveBeenCalledWith(
        { id: HOST, key: DESC.slice(5) },
        CHALLENGE,
        options,
        undefined,
      ),
    );
    const done = await screen.findByTestId("host-link-done");
    expect(done).toHaveTextContent(/not trusted yet/i);
    expect(done).toHaveTextContent(/approve.*laptop/i);
    expect(done).not.toHaveTextContent("passkey approve");
    expect(screen.getByRole("button", { name: /continue/i })).toBeEnabled();
    expect(screen.getByLabelText("Device verification code")).toHaveTextContent(
      "cred-id-123".slice(-12),
    );
    expect(control.unlockHost).not.toHaveBeenCalled();
    expect(activeHost()?.id).toBe(HOST);
  });

  it("connects a fresh browser with an already approved passkey from the laptop setup link", async () => {
    window.location.hash = hash;
    control.unlockHost.mockResolvedValue(undefined);
    const user = userEvent.setup();
    const { queryClient } = renderWithProviders(<HostLink />);
    queryClient.setQueryData(["host-delegation", HOST], null);
    const unlock = await screen.findByRole("button", {
      name: "Use approved passkey",
    });
    expect(unlock).toBeDisabled();
    await user.click(screen.getByRole("checkbox"));
    await user.click(unlock);
    await waitFor(() =>
      expect(queryClient.getQueryState(["host-delegation", HOST])?.isInvalidated)
        .toBe(true),
    );
    expect(activeHost()?.id).toBe(HOST);
    expect(control.unlockHost).toHaveBeenCalledWith(HOST);
    expect(control.finishEnrollment).not.toHaveBeenCalled();
  });

  it("keeps an existing browser-key connection if the new passkey is not approved", async () => {
    await pairHost(DESC, "Laptop", "browser-key");
    window.location.hash = hash;
    control.finishEnrollment.mockResolvedValue("cred-id-123");
    control.unlockHost.mockRejectedValue(new Error("Waiting for approval"));
    const user = userEvent.setup();
    renderWithProviders(<HostLink />);
    await user.click(
      await screen.findByRole("button", { name: "Create passkey" }),
    );
    await user.click(await screen.findByRole("button", { name: /continue/i }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Waiting for approval",
    );
    expect(activeHost()?.authMode).toBe("browser-key");
    expect(control.closePrivateChannels).not.toHaveBeenCalled();
  });

  it.each([false, true])(
    "binds the vouch to the captured host (mismatched key: %s)",
    async (mismatch) => {
      const { pairHost, activeHost } = await import("@/lib/hostPairing");
      const { makeTestHost } = await import("@/lib/testHost");
      // The colleague's OWN host is paired and active; the invite link names a
      // different host (the session owner's).
      const own = await makeTestHost();
      await pairHost(own.descriptor, "alice-laptop");
      window.location.hash = hash;
      const vouch = {
        statement: { p: "cGF5bG9hZA", s: "c2ln" },
        host_key: mismatch ? DESC.slice(5) : own.key,
      };
      control.passkeyVouch.mockResolvedValue(vouch);
      // The real finishEnrollment applies the vouch factory AFTER the passkey
      // exists and submits the result; the mock mirrors that contract.
      control.finishEnrollment.mockImplementation(
        async (
          _host: unknown,
          _c: unknown,
          _o: unknown,
          vouchFor?: unknown,
        ) => {
          const proof = await (vouchFor as (id: string) => Promise<unknown>)(
            "cred-id-456",
          );
          expect(proof).toBe(vouch);
          return "cred-id-456";
        },
      );
      const user = userEvent.setup();
      renderWithProviders(<HostLink />);
      expect(await screen.findByTestId("link-host-id")).toHaveTextContent(HOST);
      await user.click(screen.getByRole("checkbox"));
      await user.click(
        await screen.findByRole("button", { name: "Create passkey" }),
      );
      // Her own host vouches for the ceremony on the owner's host — binding
      // the passkey id her authenticator produced — before anything is
      // submitted.
      await waitFor(() =>
        expect(control.passkeyVouch).toHaveBeenCalledWith(
          HOST,
          CHALLENGE,
          expect.any(String),
          own.id,
        ),
      );
      if (mismatch) {
        expect(await screen.findByRole("alert")).toHaveTextContent(
          "different host",
        );
        expect(screen.queryByTestId("host-link-done")).not.toBeInTheDocument();
        unpairHost(own.id);
        return;
      }
      // Her side shows the pairwise code derived from both host keys.
      const code = await screen.findByTestId("pair-code");
      expect(code).toHaveTextContent(/ /);
      expect(activeHost()?.id).toBe(HOST);
      // Leave the pairing store clean for later tests.
      unpairHost(HOST);
      unpairHost(own.id);
    },
  );

  it("shows a cancelled or refused registration without pairing anything", async () => {
    window.location.hash = hash;
    control.finishEnrollment.mockRejectedValue(
      new Error("The passkey prompt was cancelled."),
    );
    const user = userEvent.setup();
    renderWithProviders(<HostLink />);
    await user.click(await screen.findByRole("checkbox"));
    await user.click(screen.getByRole("button", { name: "Create passkey" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(/cancelled/);
    expect(activeHost()).toBeNull();
  });

  it("shows no approval command when the host's completion does not match", async () => {
    window.location.hash = hash;
    control.finishEnrollment.mockRejectedValue(
      new Error(
        "The host's answer does not match what was requested (registration).",
      ),
    );
    const user = userEvent.setup();
    renderWithProviders(<HostLink />);
    await user.click(await screen.findByRole("checkbox"));
    await user.click(screen.getByRole("button", { name: "Create passkey" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      /does not match/,
    );
    expect(screen.queryByTestId("host-link-done")).toBeNull();
    expect(activeHost()).toBeNull();
  });
});
