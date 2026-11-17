import { Plugin } from "@opencode/plugin/tui";
import { onCleanup } from "solid-js";
import { closeSync, openSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { observe } from "./common.js";
import { observeReadiness } from "./readiness.js";

export default Plugin.define({
  id: "conversations.identity.v2",
  setup(context) {
    const state = {};
    if (context.options.handoff) startHandoff(context, context.options.handoff);
    return context.ui.slot({
      append: "app",
      render: () => {
        const dispose = observe(
          () => {
            const route = context.ui.router.current();
            return route.type === "session" ? route.sessionID : undefined;
          },
          id => context.data.session.get(id),
          context.options,
          state,
        );
        onCleanup(dispose);
        if (context.options.readinessPath) onCleanup(observeReadiness(context, context.options, state));
        return null;
      },
    });
  },
});

// The TUI's submit path switches a session to the TUI's own model selection, and
// it has no model flag. So the handoff session is created on the chosen model and
// its first prompt is admitted directly; the TUI then follows the session's model.
export function startHandoff(context, handoff) {
  // OpenCode sets this plugin up more than once per launch. Only the first claim acts.
  try {
    closeSync(openSync(handoff.claimPath, "wx", 0o600));
  } catch (_) {
    return;
  }
  const report = result => {
    const temporary = `${handoff.resultPath}.${randomUUID()}`;
    try {
      writeFileSync(temporary, JSON.stringify(result), { mode: 0o600, flag: "wx" });
      renameSync(temporary, handoff.resultPath);
    } finally {
      try { rmSync(temporary, { force: true }); } catch (_) {}
    }
  };
  // Set once a session exists, so a later failure does not hide a live child.
  let sessionID;
  return (async () => {
    const separator = handoff.model.indexOf("/");
    const model = {
      providerID: handoff.model.slice(0, separator),
      id: handoff.model.slice(separator + 1),
      ...(handoff.variant ? { variant: handoff.variant } : {}),
    };
    const location = context.location;
    const matches = m => m.providerID === model.providerID && m.id === model.id && m.enabled !== false;
    const found = (await catalog(context, location, matches, handoff.catalogTimeoutMs ?? 20000)).find(matches);
    if (!found) {
      throw new Error(`Model ${handoff.model} is not available in OpenCode on this computer. Run 'opencode models' to list them, then retry with a new key.`);
    }
    if (model.variant && !(found.variants ?? []).some(v => v.id === model.variant)) {
      const known = (found.variants ?? []).map(v => v.id);
      throw new Error(`Model ${handoff.model} has no variant ${model.variant}. ${known.length ? `Use one of ${known.join(", ")}` : "Omit the variant"}, then retry with a new key.`);
    }
    // A launcher such as attached adk creates the session (with its agent and profile)
    // and opens the TUI on it. Use that one, or the identity binding and the brief
    // would end up in different sessions.
    const opened = launchedSession(process.argv);
    let session;
    if (opened) {
      sessionID = opened;
      await context.client.session.switchModel({ sessionID: opened, model });
      session = { id: opened };
    } else {
      // The TUI's default agent is the first visible primary one. Creating the session
      // with it keeps the first human submit from switching agents (and agent models).
      const agent = (context.data.location.agent.list(location) ?? []).find(a => a.mode !== "subagent" && !a.hidden);
      const created = await context.client.session.create({ location, model, ...(agent ? { agent: agent.id } : {}) });
      session = created?.data ?? created;
      sessionID = session.id;
    }
    // A TUI on its own server shares its environment with new sessions; an
    // attached TUI (--server) does not. Match it so tools see the same environment.
    if (!process.argv.some(arg => arg === "--server" || arg.startsWith("--server="))) {
      const variables = Object.fromEntries(Object.entries(process.env).filter(([key, value]) =>
        value !== undefined && key !== "OPENCODE_PASSWORD" && key !== "OPENCODE_SERVER_PASSWORD"));
      await context.client.session.environment({ sessionID: session.id, variables });
    }
    context.ui.router.navigate({ type: "session", sessionID: session.id });
    await context.client.session.prompt({ sessionID: session.id, text: handoff.prompt });
    report({ ok: true });
  })().catch(error => {
    const message = String(error?.message ?? error).slice(0, 500);
    try { report(sessionID ? { error: message, sessionID } : { error: message }); } catch (_) {}
    context.ui.toast.show({ variant: "error", title: "Handoff did not start", message });
  });
}

export function launchedSession(argv) {
  for (let index = 0; index < argv.length; index++) {
    const word = argv[index];
    if (word === "--") return undefined;
    if (word === "--session" || word === "-s") return argv[index + 1];
    if (word.startsWith("--session=")) return word.slice("--session=".length);
  }
  return undefined;
}

async function catalog(context, location, matches, timeoutMs) {
  // The catalog fills asynchronously after startup, possibly one provider at a time.
  let models = [];
  for (const deadline = Date.now() + timeoutMs; Date.now() < deadline;) {
    try {
      const listed = await context.client.model.list({ location });
      models = listed?.data ?? listed ?? [];
      if (models.some(matches)) return models;
    } catch (_) {}
    await new Promise(resolve => setTimeout(resolve, 200));
  }
  if (!models.length) {
    throw new Error("OpenCode's model catalog did not load within 20 seconds. Check the OpenCode server, then retry with a new key.");
  }
  return models;
}
