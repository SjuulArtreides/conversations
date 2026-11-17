import * as React from "react";
import { pairedHost } from "./hostPairing";

/**
 * How this browser reaches one paired host: standard `RTCIceServer` entries
 * (STUN, TURN) and an ICE transport policy, set by the user here and kept only
 * in this browser, per paired host. They are network preferences, not
 * authority: nothing here touches a host's pinned key, grants or delegation,
 * nothing is sent to the Conversations server, and there are no built-in
 * servers. Empty means direct (host) candidates only.
 *
 * The shape is the browser form of the native client's
 * `transport.ice_servers` / `ice_transport_policy` settings, with the same
 * limits; each side accepts only the URL schemes and transports it handles.
 */

const STORAGE_KEY = "conversations.hostNetwork";
export const MAX_ICE_SERVERS = 4;
export const MAX_ICE_URLS = 4;
export const MAX_ICE_FIELD = 256;

export interface HostNetwork {
  /** Validated; `urls` is a string or a list, as in `RTCIceServer`. */
  iceServers: RTCIceServer[];
  iceTransportPolicy: RTCIceTransportPolicy;
}

/** What `RTCPeerConnection` receives for a host. `revision` changes with every
 * save or clear of that host's settings; compare it for equality only. */
export interface HostRtcConfig extends HostNetwork {
  revision: string;
}

/** A bounded message that never repeats a URL, username or credential. */
export class HostNetworkError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "HostNetworkError";
  }
}

const DIRECT: HostNetwork = Object.freeze({
  iceServers: [],
  iceTransportPolicy: "all",
}) as HostNetwork;

/** A host's stored entry. A cleared host keeps only its revision, so a
 * revision is never reused for different settings, also across tabs. */
interface Entry {
  rev: number;
  network: HostNetwork | null;
}

const isObject = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

const onlyKeys = (value: Record<string, unknown>, keys: string[]) => {
  if (Object.keys(value).some((key) => !keys.includes(key)))
    throw new HostNetworkError("Network settings have an unexpected field.");
};

const utf8Length = (text: string) => new TextEncoder().encode(text).length;
// eslint-disable-next-line no-control-regex
const CONTROL = /[\u0000-\u001f\u007f-\u009f]/;

const HOST_PORT =
  /^(?:\[[0-9A-Fa-f:.]+\]|[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?)*)(?::(\d{1,5}))?$/;

/** The URL's scheme kind, after checking it is one browsers handle. */
function checkUrl(url: unknown): "stun" | "turn" {
  if (typeof url !== "string" || utf8Length(url) > MAX_ICE_FIELD)
    throw new HostNetworkError(
      `Each ICE URL must be text of at most ${MAX_ICE_FIELD} bytes.`,
    );
  const colon = url.indexOf(":");
  const scheme = colon > 0 ? url.slice(0, colon).toLowerCase() : "";
  // `stuns:` is defined but not handled by browsers' RTCPeerConnection.
  if (!["stun", "turn", "turns"].includes(scheme))
    throw new HostNetworkError("Unsupported ICE URL scheme (use stun:, turn: or turns:).");
  const kind = scheme === "stun" ? "stun" : "turn";
  const rest = url.slice(colon + 1);
  const [address, query, ...more] = rest.split("?");
  if (more.length > 0) throw new HostNetworkError("Malformed ICE URL.");
  if (query !== undefined) {
    const transport = /^transport=(.*)$/.exec(query);
    if (kind !== "turn" || !transport)
      throw new HostNetworkError("Malformed ICE URL.");
    if (!["udp", "tcp"].includes(transport[1]))
      throw new HostNetworkError("Unsupported TURN transport (use udp or tcp).");
  }
  const hostPort = HOST_PORT.exec(address);
  const port = hostPort?.[1];
  if (!hostPort || (port !== undefined && !(+port >= 1 && +port <= 65535)))
    throw new HostNetworkError("Malformed ICE URL.");
  return kind;
}

const validField = (value: unknown) =>
  typeof value === "string" &&
  value.length > 0 &&
  utf8Length(value) <= MAX_ICE_FIELD &&
  !CONTROL.test(value);

function checkServer(server: unknown): RTCIceServer & { turn: boolean } {
  if (!isObject(server))
    throw new HostNetworkError("Each ICE server must be an object.");
  onlyKeys(server, ["urls", "username", "credential"]);
  const urls = server.urls;
  const list = Array.isArray(urls) ? urls : [urls];
  if (list.length < 1 || list.length > MAX_ICE_URLS)
    throw new HostNetworkError(`Each ICE server needs 1-${MAX_ICE_URLS} URLs.`);
  const kinds = list.map(checkUrl);
  const turn = kinds.includes("turn");
  const hasUser = server.username !== undefined;
  const hasCredential = server.credential !== undefined;
  if (turn && !(validField(server.username) && validField(server.credential)))
    throw new HostNetworkError(
      `A TURN server needs a username and credential of 1-${MAX_ICE_FIELD} printable bytes.`,
    );
  if (!turn && (hasUser || hasCredential))
    throw new HostNetworkError("A STUN server takes no credentials.");
  const out: RTCIceServer = { urls: urls as string | string[] };
  if (turn) {
    out.username = server.username as string;
    out.credential = server.credential as string;
  }
  return Object.assign(out, { turn });
}

/** Check settings against what this browser's RTCPeerConnection handles. */
export function validateHostNetwork(input: unknown): HostNetwork {
  if (!isObject(input))
    throw new HostNetworkError("Network settings must be an object.");
  onlyKeys(input, ["iceServers", "iceTransportPolicy"]);
  const policy = input.iceTransportPolicy ?? "all";
  if (policy !== "all" && policy !== "relay")
    throw new HostNetworkError('Unsupported transport policy (use "all" or "relay").');
  if (!Array.isArray(input.iceServers) || input.iceServers.length > MAX_ICE_SERVERS)
    throw new HostNetworkError(`Use at most ${MAX_ICE_SERVERS} ICE servers.`);
  const checked = input.iceServers.map(checkServer);
  if (policy === "relay" && !checked.some((server) => server.turn))
    throw new HostNetworkError("Relay needs at least one TURN server.");
  return {
    iceServers: checked.map(({ turn: _turn, ...server }) => server),
    iceTransportPolicy: policy,
  };
}

const CORRUPT = () =>
  new HostNetworkError(
    "The saved network settings for this host are invalid; fix or clear them.",
  );

/** All saved entries, or a HostNetworkError when the store is corrupt. */
function readAll(): Record<string, unknown> | HostNetworkError {
  const raw = localStorage.getItem(STORAGE_KEY);
  if (raw === null) return {};
  try {
    const parsed: unknown = JSON.parse(raw);
    return isObject(parsed) ? parsed : CORRUPT();
  } catch {
    return CORRUPT();
  }
}

function readEntry(hostId: string): Entry | null {
  const all = readAll();
  if (all instanceof HostNetworkError) throw all;
  const entry = all[hostId];
  if (entry === undefined) return null;
  if (
    !isObject(entry) ||
    !Number.isSafeInteger(entry.rev) ||
    (entry.rev as number) < 1
  )
    throw CORRUPT();
  if (entry.network === undefined && Object.keys(entry).length === 1)
    return { rev: entry.rev as number, network: null };
  try {
    return { rev: entry.rev as number, network: validateHostNetwork(entry.network) };
  } catch {
    throw CORRUPT();
  }
}

const listeners = new Set<() => void>();

function writeAll(all: Record<string, unknown>) {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(all));
  listeners.forEach((listener) => listener());
}

function nextRev(hostId: string, all: Record<string, unknown>) {
  const entry = all[hostId];
  return (isObject(entry) && Number.isSafeInteger(entry.rev) ? (entry.rev as number) : 0) + 1;
}

/** This host's network settings: saved, or direct only. Throws on corrupt
 * saved settings rather than silently connecting another way. */
export function hostNetwork(hostId: string): HostNetwork {
  return readEntry(hostId)?.network ?? DIRECT;
}

/** The configuration to open a connection to `hostId` with. */
export function hostRtcConfig(hostId: string): HostRtcConfig {
  const entry = readEntry(hostId);
  const network = entry?.network ?? DIRECT;
  return {
    iceServers: [...network.iceServers],
    iceTransportPolicy: network.iceTransportPolicy,
    revision: `${hostId}:${entry?.rev ?? 0}`,
  };
}

/** Validate and save `input` for a paired host. */
export function saveHostNetwork(hostId: string, input: unknown): HostNetwork {
  if (!pairedHost(hostId))
    throw new HostNetworkError("That host is not paired with this browser.");
  const network = validateHostNetwork(input);
  const all = readAll();
  const base = all instanceof HostNetworkError ? {} : all;
  writeAll({ ...base, [hostId]: { rev: nextRev(hostId, base), network } });
  return network;
}

/** Forget a host's settings (back to direct only). Also repairs a corrupt
 * store. */
export function clearHostNetwork(hostId: string) {
  const all = readAll();
  const base = all instanceof HostNetworkError ? {} : all;
  writeAll({ ...base, [hostId]: { rev: nextRev(hostId, base) } });
}

/** Notified on every save or clear here, and on changes from other tabs. */
export function subscribeHostNetwork(listener: () => void): () => void {
  listeners.add(listener);
  const onStorage = (event: StorageEvent) => {
    if (event.key === STORAGE_KEY || event.key === null) listener();
  };
  window.addEventListener("storage", onStorage);
  return () => {
    listeners.delete(listener);
    window.removeEventListener("storage", onStorage);
  };
}

let snapshot: { raw: string | null; hostId: string; value: HostNetwork | HostNetworkError } | null =
  null;

function current(hostId: string): HostNetwork | HostNetworkError {
  const raw = localStorage.getItem(STORAGE_KEY);
  if (snapshot && snapshot.raw === raw && snapshot.hostId === hostId) return snapshot.value;
  let value: HostNetwork | HostNetworkError;
  try {
    value = hostNetwork(hostId);
  } catch (error) {
    value = error as HostNetworkError;
  }
  snapshot = { raw, hostId, value };
  return value;
}

/** React view of one host's settings, or the error that blocks connecting. */
export function useHostNetwork(hostId: string): HostNetwork | HostNetworkError {
  return React.useSyncExternalStore(
    subscribeHostNetwork,
    () => current(hostId),
    () => DIRECT,
  );
}
