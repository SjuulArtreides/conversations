import "./readiness.test.mjs";
// Run with node --experimental-vm-modules test.mjs /path/to/solid-js/dist/solid.js.
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

const root = path.dirname(fileURLToPath(import.meta.url));
const context = vm.createContext({ console, process, setTimeout, clearTimeout });
const cache = new Map();
async function moduleAt(file) {
  if (cache.has(file)) return cache.get(file);
  const mod = new vm.SourceTextModule(fs.readFileSync(file, "utf8"), { context, identifier: file });
  cache.set(file, mod);
  await mod.link(async (specifier, parent) => {
    if (specifier === "solid-js") return moduleAt(process.argv[2]);
    if (specifier === "@opencode/plugin/tui") {
      return new vm.SyntheticModule(["Plugin"], function () {
        this.setExport("Plugin", { define: x => x });
      }, { context });
    }
    if (specifier.startsWith("node:")) {
      const exports = await import(specifier);
      return new vm.SyntheticModule(Object.keys(exports), function () {
        for (const key of Object.keys(exports)) this.setExport(key, exports[key]);
      }, { context });
    }
    return moduleAt(path.resolve(path.dirname(parent.identifier), specifier));
  });
  return mod;
}
const solid = await moduleAt(process.argv[2]);
await solid.evaluate();
const { createRoot, createSignal } = solid.namespace;
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "conversations-bridge-test-"));
try {
  for (const version of process.argv[3] ? [Number(process.argv[3])] : [1, 2]) {
    const mod = await moduleAt(path.join(root, `v${version}.js`));
    await mod.evaluate();
    const plugin = mod.namespace.default;
    const sessions = new Map();
    const cleanups = [];
    const read = launch => {
      const file = path.join(tmp, `${version}-${launch}.json`);
      return fs.existsSync(file) ? JSON.parse(fs.readFileSync(file, "utf8")) : undefined;
    };
    async function terminal(launch, knownID) {
      let remount;
      let setRoute, setSession;
      let route, getSession;
      createRoot(dispose => {
        [route, setRoute] = createSignal(undefined);
        [getSession, setSession] = createSignal(0);
        cleanups.push(dispose);
      });
      const get = id => { getSession(); return sessions.get(id); };
      const options = { launchID: launch, reportPath: path.join(tmp, `${version}-${launch}.json`), knownID };
      if (version === 1) {
        await plugin.tui({
          route: { get current() { return route() ? { name: "session", params: { sessionID: route() } } : { name: "home" }; } },
          state: { session: { get } }, lifecycle: { onDispose: f => cleanups.push(f) },
        }, options);
      } else {
        let render, stop;
        remount = () => { stop?.(); createRoot(d => { stop = d; render(); }); };
        const dispose = plugin.setup({ options,
          ui: { router: { current: () => route() ? { type: "session", sessionID: route() } : { type: "home" } },
            slot: slot => { render = slot.render; remount(); return () => stop?.(); } },
          data: { session: { get } },
        });
        cleanups.push(dispose);
      }
      return { route: setRoute, refresh: () => setSession(n => n + 1), remount };
    }
    // Both terminals share the same session cache/server and directory.
    const a = await terminal("a");
    const b = await terminal("b");
    sessions.set("native-a", { id: "native-a", title: "First subject" });
    sessions.set("native-b", { id: "native-b", title: "Second subject" });
    a.refresh(); b.refresh(); // Creation/cache update precedes route readiness.
    assert.equal(read("a"), undefined);
    a.route("native-a"); b.route("native-b");
    assert.equal(read("a")?.nativeID, "native-a", `V${version}: fresh route must bind exact identity`);
    assert.equal(read("b")?.title, "Second subject");
    sessions.set("native-a", { id: "native-a", title: "Revised subject" });
    a.refresh(); b.refresh();
    assert.equal(read("a").title, "Revised subject");
    assert.equal(read("b").nativeID, "native-b");
    sessions.set("child", { id: "child", parentID: "native-a", title: "Child subject" });
    a.route("child");
    assert.equal(read("a").nativeID, "native-a");
    a.route("native-b");
    assert.equal(read("a").nativeID, "native-a", "navigation must not rebind transcript");
    a.remount?.();
    assert.equal(read("a").nativeID, "native-a", "slot remount must not rebind transcript");
    const fresh = await terminal("late");
    fresh.route("child");
    assert.equal(read("late"), undefined, "a child route cannot establish a root binding");
    fresh.route("late-root");
    sessions.set("late-root", { id: "late-root", title: "New session - 2026-10-21T10:00:00.000Z" });
    fresh.refresh();
    assert.equal(read("late").nativeID, "late-root");
    assert.equal(read("late").title, null, "exact native timestamp fallback is not a subject");
    sessions.set("late-root", { id: "late-root", title: "Delayed generated title" });
    fresh.refresh();
    assert.equal(read("late").title, "Delayed generated title");
    sessions.set("late-root", { id: "late-root", title: "New session - a legitimate subject" });
    fresh.refresh();
    assert.equal(read("late").title, "New session - a legitimate subject");
    const resumed = await terminal("resume", "native-a");
    resumed.route("native-b");
    assert.equal(read("resume"), undefined, "known identity must remain authoritative");
    resumed.route("native-a");
    assert.equal(read("resume").nativeID, "native-a");
    for (const cleanup of cleanups.reverse()) cleanup?.();
    console.log(`V${version}: route identity, isolation, delayed titles and resume authority passed`);
  }
  if (!process.argv[3] || process.argv[3] === "2") {
    const { startHandoff } = (await moduleAt(path.join(root, "v2.js"))).namespace;
    const catalog = [{ providerID: "anthropic", id: "claude-haiku-4-5", enabled: true, variants: [{ id: "high" }, { id: "max" }] }];
    const fixture = name => {
      const calls = [];
      const toasts = [];
      const handoff = { model: "anthropic/claude-haiku-4-5", variant: "high", prompt: "Brief",
        claimPath: path.join(tmp, `${name}.claim`), resultPath: path.join(tmp, `${name}.json`), catalogTimeoutMs: 300 };
      const context = {
        location: { directory: "/work" },
        data: { location: { agent: { list: () => [
          { id: "general", mode: "subagent" }, { id: "title", mode: "primary", hidden: true }, { id: "build", mode: "primary" }] } } },
        client: {
          model: { list: async () => ({ data: catalog }) },
          session: {
            create: async input => { calls.push(["create", input]); return { data: { id: "ses_child" } }; },
            switchModel: async input => { calls.push(["switchModel", input]); },
            environment: async input => { calls.push(["environment", Object.keys(input.variables)]); },
            prompt: async input => { calls.push(["prompt", input]); },
          },
        },
        ui: { router: { navigate: route => calls.push(["navigate", route]) }, toast: { show: toast => toasts.push(toast) } },
      };
      const result = () => JSON.parse(fs.readFileSync(handoff.resultPath, "utf8"));
      return { calls, toasts, handoff, context, result };
    };
    // Values built inside the VM realm compare by structure, not prototype.
    const plain = value => JSON.parse(JSON.stringify(value));
    process.env.OPENCODE_PASSWORD = "not-for-sessions";
    const ok = fixture("ok");
    const first = startHandoff(ok.context, ok.handoff);
    assert.equal(startHandoff(ok.context, ok.handoff), undefined, "a second plugin setup must not act");
    await first;
    assert.deepEqual(plain(ok.calls.map(([step]) => step)), ["create", "environment", "navigate", "prompt"]);
    assert.deepEqual(plain(ok.calls[0][1]), { location: { directory: "/work" },
      model: { providerID: "anthropic", id: "claude-haiku-4-5", variant: "high" }, agent: "build" });
    assert.ok(!ok.calls[1][1].includes("OPENCODE_PASSWORD"), "server passwords never reach the session");
    assert.deepEqual(plain(ok.calls[3][1]), { sessionID: "ses_child", text: "Brief" });
    assert.deepEqual(ok.result(), { ok: true });
    delete process.env.OPENCODE_PASSWORD;

    // Attached adk opens the TUI on a session it created; the handoff must use that one.
    const { launchedSession } = (await moduleAt(path.join(root, "v2.js"))).namespace;
    assert.equal(launchedSession(["/bin/opencode", "--server=http://h", "--session", "ses_adk", "--auto"]), "ses_adk");
    assert.equal(launchedSession(["/bin/opencode", "--session=ses_adk"]), "ses_adk");
    assert.equal(launchedSession(["/bin/opencode", "--auto"]), undefined);
    const attached = fixture("attached");
    const argv = process.argv;
    process.argv = ["/bin/opencode", "--server=http://h", "--session", "ses_adk"];
    try {
      await startHandoff(attached.context, attached.handoff);
    } finally {
      process.argv = argv;
    }
    assert.deepEqual(plain(attached.calls), [
      ["switchModel", { sessionID: "ses_adk", model: { providerID: "anthropic", id: "claude-haiku-4-5", variant: "high" } }],
      ["navigate", { type: "session", sessionID: "ses_adk" }],
      ["prompt", { sessionID: "ses_adk", text: "Brief" }],
    ], "no new session, and no environment push for an attached TUI");

    // A failure after the session exists names it, so the host keeps the live child.
    const failing = fixture("failing");
    failing.context.client.session.prompt = async () => { throw new Error("Provider rejected the prompt"); };
    await startHandoff(failing.context, failing.handoff);
    assert.deepEqual(plain(failing.result()), { error: "Provider rejected the prompt", sessionID: "ses_child" });

    // Providers can load one at a time: keep looking until the model appears.
    const late = fixture("late");
    let listed = 0;
    late.context.client.model.list = async () => ({ data: listed++ ? catalog : [{ providerID: "openai", id: "other", enabled: true }] });
    await startHandoff(late.context, late.handoff);
    assert.deepEqual(plain(late.result()), { ok: true });

    // A host that gave up first holds the claim, so a late plugin never starts the task.
    const cancelled = fixture("cancelled");
    fs.writeFileSync(cancelled.handoff.claimPath, "");
    assert.equal(startHandoff(cancelled.context, cancelled.handoff), undefined);
    assert.deepEqual(plain(cancelled.calls), []);

    const missing = fixture("missing");
    missing.handoff.model = "anthropic/claude-unknown";
    await startHandoff(missing.context, missing.handoff);
    assert.deepEqual(plain(missing.calls), [], "an unknown model must not start a session on another model");
    assert.match(missing.result().error, /anthropic\/claude-unknown is not available.*opencode models/);
    assert.equal(missing.result().sessionID, undefined, "no session was created, so the checkpoint can go");
    assert.equal(missing.toasts.length, 1);

    const variant = fixture("variant");
    variant.handoff.variant = "xhigh";
    await startHandoff(variant.context, variant.handoff);
    assert.deepEqual(plain(variant.calls), []);
    assert.match(variant.result().error, /no variant xhigh\. Use one of high, max/);
    console.log("V2: model handoff runs once, on the chosen model and default agent, and reports failures");
  }
} finally {
  fs.rmSync(tmp, { recursive: true, force: true });
}
