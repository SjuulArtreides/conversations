import {
  DELEGATION_MARGIN_SECS,
  getDelegation,
  loadBrowserKey,
} from "./hostAuth";
import { hostControl, HostControlError } from "./hostControl";
import { pairedHost } from "./hostPairing";

/** Inspect signed host metadata without writes, sessions, or passkey prompts. */
export async function inspectHostAccess(
  hostId: string,
): Promise<{ canRead: boolean; canStart: boolean }> {
  const host = pairedHost(hostId);
  if (!host) {
    throw new HostControlError(
      "Pair this machine before checking its access.",
      "not_paired",
    );
  }
  const credentials = await hostControl.listAccess(hostId);
  const key = await loadBrowserKey();
  if (host.authMode === "browser-key") {
    const credential = credentials.find(
      (entry) =>
        entry.kid === key.kid &&
        entry.alg === "es256" &&
        entry.kind === "owner" &&
        !entry.revoked,
    );
    if (!credential) {
      throw new HostControlError(
        "This browser does not have an approved owner credential on the machine.",
        "refused",
      );
    }
    const permits = (cap: string) =>
      credential.grants.some(
        (grant) => grant.cap === cap && grant.session === "*",
      );
    return {
      canRead: permits("history.read"),
      canStart: permits("session.start"),
    };
  }
  const delegation = await getDelegation(hostId);
  if (
    !delegation ||
    delegation.hostId !== hostId ||
    delegation.kid !== key.kid ||
    delegation.notAfter - Math.floor(Date.now() / 1000) < DELEGATION_MARGIN_SECS
  ) {
    throw new HostControlError(
      "Unlock this browser with your passkey to check its access.",
      "locked",
    );
  }
  if (!delegation.parentKid) {
    throw new HostControlError(
      "Unlock this browser again with your passkey to renew its access details.",
      "locked",
    );
  }
  const parent = credentials.find(
    (entry) =>
      entry.kid === delegation.parentKid &&
      entry.kind === "owner" &&
      !entry.revoked,
  );
  if (!parent) {
    throw new HostControlError(
      "The passkey that approved this browser is no longer an approved owner credential.",
      "refused",
    );
  }
  const permits = (cap: string) =>
    !!delegation.caps?.includes(cap) &&
    parent.grants.some((grant) => grant.cap === cap && grant.session === "*");
  return {
    canRead: permits("history.read"),
    canStart: permits("session.start"),
  };
}
