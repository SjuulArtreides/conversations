import { beforeEach, describe, expect, it } from "vitest";
import {
  activeHost,
  pairedHost,
  pairHost,
  unpairHost,
  selectHost,
} from "./hostPairing";
import { hostNetwork, saveHostNetwork } from "./hostNetwork";
import fixture from "./fixtures/host_statements.json";

const HOST = fixture.host.id;
const SECRET = "turn-secret-Zq81";

beforeEach(() => {
  localStorage.clear();
});

describe("hostPairing", () => {
  it("does not select an unpaired host", async () => {
    await pairHost(fixture.host.descriptor, "laptop");
    selectHost("z".repeat(43));
    expect(activeHost()?.id).toBe(HOST);
  });
  it("drops a host's network settings when the host is unpaired, and only then", async () => {
    await pairHost(fixture.host.descriptor, "laptop");
    const pin = pairedHost(HOST);
    saveHostNetwork(HOST, {
      iceServers: [
        { urls: "turn:turn.example.com", username: "u", credential: SECRET },
      ],
    });
    expect(pairedHost(HOST)).toEqual(pin);
    expect(activeHost()?.id).toBe(HOST);

    unpairHost(HOST);
    expect(pairedHost(HOST)).toBeNull();
    expect(
      localStorage.getItem("conversations.hostNetwork") ?? "",
    ).not.toContain(SECRET);

    // Pairing again starts from direct connections, not the old relay.
    await pairHost(fixture.host.descriptor, "laptop");
    expect(hostNetwork(HOST)).toEqual({
      iceServers: [],
      iceTransportPolicy: "all",
    });
  });
});
