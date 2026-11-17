import "fake-indexeddb/auto";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  clearHostNetwork,
  HostNetworkError,
  hostNetwork,
  hostRtcConfig,
  saveHostNetwork,
  subscribeHostNetwork,
  validateHostNetwork,
} from "./hostNetwork";
import { pairHost } from "./hostPairing";
import fixture from "./fixtures/host_statements.json";

const HOST = fixture.host.id;
const OTHER = "A".repeat(43);
const SECRET = "turn-secret-Zq81";
const TURN = {
  urls: "turn:turn.example.com:3478?transport=udp",
  username: "alice",
  credential: SECRET,
};
const NETWORK_KEY = "conversations.hostNetwork";
const PAIRING_KEY = "conversations.hosts";

beforeEach(async () => {
  localStorage.clear();
  await pairHost(fixture.host.descriptor, "laptop");
});

afterEach(() => {
  vi.restoreAllMocks();
});

const refused = (input: unknown): string => {
  try {
    validateHostNetwork(input);
  } catch (error) {
    expect(error).toBeInstanceOf(HostNetworkError);
    return (error as Error).message;
  }
  throw new Error(`accepted ${JSON.stringify(input)}`);
};

describe("validateHostNetwork", () => {
  it("accepts the standard RTCIceServer shape and the transport policy enum", () => {
    expect(validateHostNetwork({ iceServers: [] })).toEqual({
      iceServers: [],
      iceTransportPolicy: "all",
    });
    const network = {
      iceServers: [
        { urls: ["stun:stun.example.com", "stun:[2001:db8::1]:3478"] },
        TURN,
        {
          urls: ["turns:turn.example.com:5349?transport=tcp", "turn:192.0.2.7"],
          username: "u",
          credential: "c",
        },
      ],
      iceTransportPolicy: "relay",
    };
    expect(validateHostNetwork(network)).toEqual(network);
  });

  it("refuses what browsers do not handle or the limits exclude, without echoing input", () => {
    const cases: [unknown, RegExp][] = [
      [null, /network settings/i],
      [{ iceServers: [], relayOnly: true }, /unexpected field/i],
      [{ iceServers: [], iceTransportPolicy: "none" }, /transport policy/i],
      [{ iceServers: [], iceTransportPolicy: "relay" }, /relay needs at least one TURN/i],
      [{ iceServers: [{ urls: "stun:a" }], iceTransportPolicy: "relay" }, /relay needs/i],
      [{ iceServers: Array(5).fill({ urls: "stun:a" }) }, /at most 4/i],
      [{ iceServers: [{ urls: [] }] }, /1-4 URLs/i],
      [{ iceServers: [{ urls: Array(5).fill("stun:a") }] }, /1-4 URLs/i],
      [{ iceServers: [{ urls: `stun:${"a".repeat(260)}` }] }, /256/],
      [{ iceServers: [{ urls: "stuns:stun.example.com" }] }, /unsupported ICE URL scheme/i],
      [{ iceServers: [{ urls: "http://turn.example.com" }] }, /unsupported ICE URL scheme/i],
      [{ iceServers: [{ ...TURN, urls: "turn:h?transport=sctp" }] }, /unsupported TURN transport/i],
      [{ iceServers: [{ urls: "stun:h?transport=udp" }] }, /malformed ICE URL/i],
      [{ iceServers: [{ ...TURN, urls: "turn:user:pw@h:3478" }] }, /malformed ICE URL/i],
      [{ iceServers: [{ ...TURN, urls: "turn://h:3478/path" }] }, /malformed ICE URL/i],
      [{ iceServers: [{ ...TURN, urls: "turn:h:70000" }] }, /malformed ICE URL/i],
      [{ iceServers: [{ urls: "turn:h:3478" }] }, /needs a username and credential/i],
      [{ iceServers: [{ ...TURN, credential: "" }] }, /needs a username and credential/i],
      [{ iceServers: [{ ...TURN, credential: "x".repeat(257) }] }, /needs a username and credential/i],
      [{ iceServers: [{ ...TURN, credential: "a\u0000b" }] }, /needs a username and credential/i],
      [{ iceServers: [{ urls: "stun:h", username: "u", credential: SECRET }] }, /STUN server takes no credentials/i],
      [{ iceServers: [{ ...TURN, credentialType: "oauth" }] }, /unexpected field/i],
    ];
    for (const [input, expected] of cases) {
      const message = refused(input);
      expect(message, JSON.stringify(input)).toMatch(expected);
      expect(message).not.toContain(SECRET);
      expect(message).not.toContain("turn.example.com");
      expect(message.length).toBeLessThan(120);
    }
  });
});

describe("per-host persisted network settings", () => {
  it("defaults to direct candidates only and persists per paired host across reloads", () => {
    expect(hostNetwork(HOST)).toEqual({ iceServers: [], iceTransportPolicy: "all" });
    const before = hostRtcConfig(HOST);
    expect(before).toMatchObject({ iceServers: [], iceTransportPolicy: "all" });

    saveHostNetwork(HOST, { iceServers: [TURN], iceTransportPolicy: "relay" });
    // A "reload": nothing but storage carries over.
    const stored = localStorage.getItem(NETWORK_KEY);
    localStorage.clear();
    localStorage.setItem(NETWORK_KEY, stored!);
    expect(hostNetwork(HOST)).toEqual({ iceServers: [TURN], iceTransportPolicy: "relay" });
    const after = hostRtcConfig(HOST);
    expect(after).toMatchObject({ iceServers: [TURN], iceTransportPolicy: "relay" });
    expect(after.revision).not.toBe(before.revision);
    expect(after.revision).not.toContain(SECRET);
    expect(hostNetwork(OTHER)).toEqual({ iceServers: [], iceTransportPolicy: "all" });
  });

  it("refuses settings for a host that is not paired", () => {
    expect(() => saveHostNetwork(OTHER, { iceServers: [] })).toThrow(/not paired/i);
    expect(localStorage.getItem(NETWORK_KEY)).toBeNull();
  });

  it("fails closed on malformed saved settings instead of connecting directly", () => {
    for (const corrupt of [
      "not json",
      JSON.stringify({ [HOST]: { rev: 1, network: { iceServers: [{ urls: "turn:h" }] } } }),
      JSON.stringify({ [HOST]: { rev: "x", network: { iceServers: [] } } }),
      JSON.stringify([1]),
    ]) {
      localStorage.setItem(NETWORK_KEY, corrupt);
      expect(() => hostRtcConfig(HOST), corrupt).toThrow(HostNetworkError);
      expect(() => hostNetwork(HOST)).toThrow(/saved network settings/i);
    }
    clearHostNetwork(HOST);
    expect(hostRtcConfig(HOST)).toMatchObject({ iceServers: [], iceTransportPolicy: "all" });
  });

  it("changes the revision on every save and clear, and notifies listeners", () => {
    const seen: string[] = [];
    const listener = vi.fn(() => seen.push(hostRtcConfig(HOST).revision));
    const stop = subscribeHostNetwork(listener);
    saveHostNetwork(HOST, { iceServers: [TURN] });
    saveHostNetwork(HOST, { iceServers: [TURN] });
    clearHostNetwork(HOST);
    stop();
    saveHostNetwork(HOST, { iceServers: [] });
    expect(listener).toHaveBeenCalledTimes(3);
    expect(new Set(seen).size).toBe(3);
  });

  it("never touches pairing pins, keys or delegations, the server, or logs", async () => {
    const pairing = localStorage.getItem(PAIRING_KEY);
    const fetchSpy = vi.spyOn(globalThis, "fetch");
    const openSpy = vi.spyOn(indexedDB, "open");
    const logs = (["log", "info", "warn", "error", "debug"] as const).map((level) =>
      vi.spyOn(console, level).mockImplementation(() => {}),
    );
    saveHostNetwork(HOST, { iceServers: [TURN], iceTransportPolicy: "relay" });
    hostRtcConfig(HOST);
    try {
      saveHostNetwork(HOST, { iceServers: [{ ...TURN, urls: "turn:bad:0" }] });
    } catch (error) {
      expect(String(error)).not.toContain(SECRET);
    }
    clearHostNetwork(HOST);
    expect(localStorage.getItem(PAIRING_KEY)).toBe(pairing);
    expect(fetchSpy).not.toHaveBeenCalled();
    expect(openSpy).not.toHaveBeenCalled();
    for (const spy of logs) expect(spy).not.toHaveBeenCalled();
  });

});
