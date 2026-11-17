import "fake-indexeddb/auto";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  forgetDelegation,
  loadBrowserKey,
  saveDelegation,
  type Delegation,
} from "./hostAuth";
import { hostControl, HostControlError } from "./hostControl";
import { inspectHostAccess } from "./hostSetup";
import { pairHost, setHostAuthMode, unpairHost } from "./hostPairing";
import { makeTestHost } from "./testHost";
import type { AccessCredential } from "./privateChannel";
vi.mock("./hostControl", async (actual) => {
  const real = await actual<typeof import("./hostControl")>();
  return { ...real, hostControl: { ...real.hostControl, listAccess: vi.fn() } };
});
let hostId: string;
let kid: string;
let credential: AccessCredential;
let parent: AccessCredential;
let delegation: Delegation;
beforeEach(async () => {
  vi.clearAllMocks();
  localStorage.clear();
  const host = await makeTestHost();
  hostId = host.id;
  kid = (await loadBrowserKey()).kid;
  await pairHost(host.descriptor, "Coder", "browser-key");
  await forgetDelegation(hostId);
  credential = {
    kid,
    label: "Browser",
    alg: "es256",
    kind: "owner",
    revoked: false,
    grants: [
      { cap: "history.read", session: "*" },
      { cap: "session.start", session: "*" },
    ],
  };
  parent = {
    ...credential,
    kid: "passkey-parent",
    alg: "rs256",
    grants: [...credential.grants],
  };
  delegation = {
    parentKid: parent.kid,
    hostId,
    kid,
    dlg: "delegation",
    notAfter: Math.floor(Date.now() / 1000) + 3600,
    caps: ["history.read", "session.start"],
  };
  vi.mocked(hostControl.listAccess).mockResolvedValue([credential, parent]);
});
describe("signed host access inspection", () => {
  it("recognizes globally approved owner access on the requested host", async () => {
    await expect(inspectHostAccess(hostId)).resolves.toEqual({
      canRead: true,
      canStart: true,
    });
    expect(hostControl.listAccess).toHaveBeenCalledWith(hostId);
  });
  it.each([
    "revoked",
    "agent",
    "another key",
    "wrong algorithm",
    "no credential",
  ])("does not claim readiness for %s", async (change) => {
    if (change === "revoked") credential.revoked = true;
    if (change === "agent") credential.kind = "agent";
    if (change === "another key") credential.kid = "other-kid";
    if (change === "wrong algorithm") credential.alg = "ed25519";
    if (change === "no credential")
      vi.mocked(hostControl.listAccess).mockResolvedValue([]);
    await expect(inspectHostAccess(hostId)).rejects.toMatchObject({
      code: "refused",
    });
  });
  it("keeps read-only and session-scoped grants from claiming start access", async () => {
    credential.grants = [
      { cap: "history.read", session: "*" },
      { cap: "session.start", session: "one-session" },
    ];
    await expect(inspectHostAccess(hostId)).resolves.toEqual({
      canRead: true,
      canStart: false,
    });
    credential.grants = [{ cap: "history.read", session: "one-session" }];
    await expect(inspectHostAccess(hostId)).resolves.toEqual({
      canRead: false,
      canStart: false,
    });
  });
  it.each(["refused", "locked", "offline", "transport", "untrusted"] as const)(
    "propagates %s instead of treating an open channel as approval",
    async (code) => {
      const error = new HostControlError("inspection failed", code);
      vi.mocked(hostControl.listAccess).mockRejectedValue(error);
      await expect(inspectHostAccess(hostId)).rejects.toBe(error);
      expect(hostControl.listAccess).toHaveBeenCalledTimes(1);
    },
  );
  it("does not inspect another host when the selected pairing is absent", async () => {
    unpairHost(hostId);
    await expect(inspectHostAccess(hostId)).rejects.toMatchObject({
      code: "not_paired",
    });
    expect(hostControl.listAccess).not.toHaveBeenCalled();
  });
  it("uses only the valid delegation's narrowed caps in passkey mode", async () => {
    setHostAuthMode(hostId, "passkey");
    delegation.caps = ["history.read"];
    await saveDelegation(delegation);
    await expect(inspectHostAccess(hostId)).resolves.toEqual({
      canRead: true,
      canStart: false,
    });
    delegation.caps = ["session.start"];
    await saveDelegation(delegation);
    await expect(inspectHostAccess(hostId)).resolves.toEqual({
      canRead: false,
      canStart: true,
    });
  });
  it("stops claiming readiness when the passkey parent loses its grants", async () => {
    setHostAuthMode(hostId, "passkey");
    await saveDelegation(delegation);
    await expect(inspectHostAccess(hostId)).resolves.toEqual({
      canRead: true,
      canStart: true,
    });
    parent.grants = [{ cap: "session.write", session: "*" }];
    await expect(inspectHostAccess(hostId)).resolves.toEqual({
      canRead: false,
      canStart: false,
    });
    parent.grants.push({ cap: "history.read", session: "*" });
    await expect(inspectHostAccess(hostId)).resolves.toEqual({
      canRead: true,
      canStart: false,
    });
  });
  it.each(["revoked", "agent", "missing", "wrong parent"])(
    "rejects a %s delegation parent instead of borrowing another owner's grants",
    async (change) => {
      setHostAuthMode(hostId, "passkey");
      if (change === "revoked") parent.revoked = true;
      if (change === "agent") parent.kind = "agent";
      if (change === "missing")
        vi.mocked(hostControl.listAccess).mockResolvedValue([credential]);
      if (change === "wrong parent") delegation.parentKid = "not-this-parent";
      await saveDelegation(delegation);
      await expect(inspectHostAccess(hostId)).rejects.toMatchObject({
        code: "refused",
      });
    },
  );
  it("requires explicit renewal for legacy delegations without a bound parent", async () => {
    setHostAuthMode(hostId, "passkey");
    delete delegation.parentKid;
    await saveDelegation(delegation);
    await expect(inspectHostAccess(hostId)).rejects.toMatchObject({
      code: "locked",
      message: expect.stringMatching(/Unlock/),
    });
  });
  it("requires global parent grants as well as exact delegated caps", async () => {
    setHostAuthMode(hostId, "passkey");
    parent.grants = [
      { cap: "history.read", session: "one-session" },
      { cap: "session.start", session: "*" },
    ];
    delegation.caps = ["history.read", "session.start:one-session"];
    await saveDelegation(delegation);
    await expect(inspectHostAccess(hostId)).resolves.toEqual({
      canRead: false,
      canStart: false,
    });
  });

  it("does not infer passkey capabilities from directly approved browser metadata", async () => {
    setHostAuthMode(hostId, "passkey");
    delegation.caps = undefined;
    await saveDelegation(delegation);
    await expect(inspectHostAccess(hostId)).resolves.toEqual({
      canRead: false,
      canStart: false,
    });
  });
  it.each(["expired", "expiring", "another key", "another host", "missing"])(
    "requires unlock for a %s delegation after signed metadata",
    async (change) => {
      setHostAuthMode(hostId, "passkey");
      if (change === "expired")
        delegation.notAfter = Math.floor(Date.now() / 1000) - 1;
      if (change === "expiring")
        delegation.notAfter = Math.floor(Date.now() / 1000) + 30;
      if (change === "another key") delegation.kid = "other-kid";
      if (change === "another host") delegation.hostId = "other-host";
      if (change !== "missing") await saveDelegation(delegation);
      await expect(inspectHostAccess(hostId)).rejects.toMatchObject({
        code: "locked",
      });
      expect(hostControl.listAccess).toHaveBeenCalledWith(hostId);
    },
  );
});
