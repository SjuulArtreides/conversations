import { describe, expect, it } from "vitest";
import {
  checkBody,
  sessionListing,
  type PrivateChannel,
} from "./privateChannel";
const entry = {
  id: "s",
  agent: "codex",
  cwd: "/work",
  title: null,
  messages: 0,
  last_activity: null,
  running: false,
};
const body = (extra: object) => ({
  kind: "sessions",
  sessions: [{ ...entry, ...extra }],
  next: null,
});
describe("private session wire compatibility", () => {
  it.each([
    {},
    { reopenable: true },
    { reopenable: false },
    { resume_mode: "native" },
    { resume_mode: "continuation" },
  ])("accepts old and new hosts: %j", (extra) => {
    expect(checkBody(body(extra))).toEqual(body(extra));
  });
  it.each([null, "other", 0])(
    "rejects invalid resume modes: %j",
    (resume_mode) => {
      expect(() => checkBody(body({ resume_mode }))).toThrow();
    },
  );
  it.each([null, "false", 0])(
    "rejects invalid reopenable values: %j",
    (reopenable) => {
      expect(() => checkBody(body({ reopenable }))).toThrow();
    },
  );
});

it.each(["unknown", "working", "background", "needs_input", "ready"])(
  "accepts native readiness %s on the private wire",
  (readiness) => {
    expect(checkBody(body({ readiness }))).toEqual(body({ readiness }));
  },
);
it.each([null, false, 3, "finished", {}, ["ready"]])(
  "rejects malformed native readiness %j",
  (readiness) => {
    expect(() => checkBody(body({ readiness }))).toThrow();
  },
);

it("negotiates readiness with old hosts and retries only an unsupported read", async () => {
  const calls: unknown[] = [];
  const operation: PrivateChannel["operation"] = async (
    _op,
    _session,
    args,
  ) => {
    calls.push(args);
    return calls.length === 1
      ? {
          status: "refused",
          detail:
            "malformed operation: unknown field `readiness`, expected one of `after`, `limit`, `running_only` at line 1 column 47",
          body: null,
        }
      : { status: "executed", detail: null, body: null };
  };
  const list = sessionListing(operation);
  await list(null, 50, true);
  await list("next", 50, true);
  expect(calls).toEqual([
    { after: null, limit: 50, running_only: true, readiness: true },
    { after: null, limit: 50, running_only: true },
    { after: "next", limit: 50, running_only: true },
  ]);
  let refused = 0;
  const denied = sessionListing(async () => {
    refused++;
    return { status: "refused", detail: "authority denied", body: null };
  });
  expect((await denied()).status).toBe("refused");
  expect(refused).toBe(1);
});
