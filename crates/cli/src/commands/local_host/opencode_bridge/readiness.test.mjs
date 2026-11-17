import assert from "node:assert/strict";
import { snapshot } from "./readiness.js";

const location = { directory: "/project" };
const root = { id: "root", location, outcome: "succeeded", time: { idle: 123 } };
const child = { id: "child", parentID: "root" };
let active = {};
let shells = [];
let permissions = [];
let forms = [];
const client = {
  session: { get: async ({sessionID}) => sessionID === "root" ? root : child, active: async () => active },
  shell: { list: async () => ({ data: shells }) },
  permission: { request: { list: async () => ({ data: permissions }) } },
  form: { list: async () => ({ data: forms }) },
};
assert.equal(await snapshot(client, "root"), "ready");
active = { root: { type: "running" } };
assert.equal(await snapshot(client, "root"), "working", "silence cannot end an active execution");
active = { child: { type: "running" } };
assert.equal(await snapshot(client, "root"), "background", "parent completion cannot hide a running child");
active = {};
shells = [{ status: "running", metadata: { sessionID: "child" } }];
assert.equal(await snapshot(client, "root"), "background", "child background shell survives child execution end");
permissions = [{ sessionID: "child" }];
assert.equal(await snapshot(client, "root"), "needs_input");
permissions = [];
forms = [{ sessionID: "root" }];
assert.equal(await snapshot(client, "root"), "needs_input");
forms = [];
shells = [];
root.outcome = "interrupted";
assert.equal(await snapshot(client, "root"), "unknown");
root.outcome = "succeeded";
root.time = {};
assert.equal(await snapshot(client, "root"), "unknown", "an absent active cache entry is not completion proof");
console.log("OpenCode native readiness snapshots passed");
