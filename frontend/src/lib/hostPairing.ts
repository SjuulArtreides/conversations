import * as React from "react";
import {
  parseDescriptor,
  pinIsConsistent,
  type PinnedHost,
} from "./hostStatements";
import { clearHostNetwork } from "./hostNetwork";

/**
 * Hosts this browser has been told to trust. A pairing pins BOTH the host's
 * public identity key and its id (the hash of that key). They come from a
 * descriptor the user copied from their own machine (`credential
 * descriptor`, or the enrollment link), never from the server or an account
 * lookup. Everything the host says is verified against the pinned key, so the
 * relay cannot substitute another host. Entries saved before keys were pinned
 * (id only) cannot verify anything and must be paired again.
 */

const STORAGE_KEY = "conversations.hosts";
const HOST_ID = /^[A-Za-z0-9_-]{43}$/;

export interface PairedHost {
  id: string;
  label: string;
  /** base64url Ed25519 public key; absent on legacy id-only entries. */
  key?: string;
  /** Missing on existing pairings: use passkey delegation. */
  authMode?: "passkey" | "browser-key";
}

export type VerifiablePairedHost = PairedHost & PinnedHost;

export const isPinned = (host: PairedHost): host is VerifiablePairedHost =>
  typeof host.key === "string";

interface PairingState {
  hosts: PairedHost[];
  active: string | null;
}

const EMPTY: PairingState = { hosts: [], active: null };
const listeners = new Set<() => void>();
let cached: { raw: string | null; state: PairingState } | null = null;

export const isValidHostId = (value: string): boolean => HOST_ID.test(value);

function read(): PairingState {
  const raw = localStorage.getItem(STORAGE_KEY);
  if (cached && cached.raw === raw) return cached.state;
  let state: PairingState;
  try {
    const parsed = JSON.parse(raw ?? "null") as Partial<PairingState> | null;
    const hosts = (parsed?.hosts ?? []).filter(
      (h): h is PairedHost =>
        typeof h?.id === "string" &&
        isValidHostId(h.id) &&
        typeof h.label === "string" &&
        (h.key === undefined || typeof h.key === "string"),
    );
    state = {
      hosts,
      active: hosts.some((h) => h.id === parsed?.active)
        ? (parsed?.active ?? null)
        : (hosts[0]?.id ?? null),
    };
  } catch {
    state = EMPTY;
  }
  cached = { raw, state };
  return state;
}

function write(state: PairingState) {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
  listeners.forEach((l) => l());
}

/** Pair from a descriptor. The id is derived from the key, never typed. */
export async function pairHost(
  descriptor: string,
  label: string,
  authMode: "passkey" | "browser-key" = "passkey",
): Promise<PairedHost> {
  let pinned: PinnedHost;
  try {
    pinned = await parseDescriptor(descriptor);
  } catch (error) {
    throw new Error((error as Error).message, { cause: error });
  }
  const host = {
    ...pinned,
    authMode,
    label: label.trim().slice(0, 64) || "My host",
  };
  const state = read();
  write({
    hosts: [...state.hosts.filter((h) => h.id !== host.id), host],
    active: host.id,
  });
  return host;
}

/** A stored pin whose key no longer hashes to its id was edited: not usable. */
export const pinIsIntact = (host: VerifiablePairedHost) =>
  pinIsConsistent(host);

export function pairedHost(id: string): PairedHost | null {
  return read().hosts.find((h) => h.id === id) ?? null;
}

/** Forget a host, and this browser's network settings for it. */
export function unpairHost(id: string) {
  clearHostNetwork(id);
  const state = read();
  const hosts = state.hosts.filter((h) => h.id !== id);
  write({
    hosts,
    active: state.active === id ? (hosts[0]?.id ?? null) : state.active,
  });
}

export function activeHost(): PairedHost | null {
  const state = read();
  return state.hosts.find((h) => h.id === state.active) ?? null;
}

/** Select only a host that this browser already trusts. */
export function selectHost(id: string) {
  const state = read();
  if (!state.hosts.some((host) => host.id === id)) return;
  write({ ...state, active: id });
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  window.addEventListener("storage", listener);
  return () => {
    listeners.delete(listener);
    window.removeEventListener("storage", listener);
  };
}

export function usePairedHosts(): PairingState {
  return React.useSyncExternalStore(subscribe, read, () => EMPTY);
}

/** Choosing direct mode grants nothing; the host still checks local enrollment. */
export function setHostAuthMode(
  id: string,
  authMode: "passkey" | "browser-key",
) {
  const state = read();
  write({
    ...state,
    hosts: state.hosts.map((host) =>
      host.id === id ? { ...host, authMode } : host,
    ),
  });
}
