import { writeFileSync, renameSync, rmSync } from "node:fs";
import { randomUUID } from "node:crypto";

// Use native snapshots, not the TUI cache: its missing-session status is "idle".
export async function snapshot(client, rootID) {
  const request = { signal: AbortSignal.timeout(2500) };
  const root = await client.session.get({ sessionID: rootID }, request);
  if (root.id !== rootID || root.parentID) return "unknown";
  const location = root.location;
  const [active, shells, permissions, forms] = await Promise.all([
    client.session.active(request),
    client.shell.list({ location }, request),
    client.permission.request.list({ location }, request),
    client.form.list({ location }, request),
  ]);
  if (!active || Array.isArray(active) || !Array.isArray(shells.data)
      || !Array.isArray(permissions.data) || !Array.isArray(forms.data)) return "unknown";
  const ancestry = new Map([[rootID, true]]);
  const belongs = async (id, depth = 0) => {
    if (typeof id !== "string" || depth > 64) throw new Error("Invalid session ancestry");
    if (ancestry.has(id)) return ancestry.get(id);
    // Break malformed cycles without treating them as another session.
    ancestry.set(id, undefined);
    const info = await client.session.get({ sessionID: id }, request);
    if (info.id !== id) throw new Error("Native session identity mismatch");
    const owned = info.parentID ? await belongs(info.parentID, depth + 1) : false;
    if (owned === undefined) throw new Error("Invalid session ancestry");
    ancestry.set(id, owned);
    return owned;
  };
  for (const item of [...permissions.data, ...forms.data]) {
    if (await belongs(item.sessionID)) return "needs_input";
  }
  let background = false;
  for (const [id, state] of Object.entries(active)) {
    if (state?.type !== "running") return "unknown";
    if (id !== rootID && await belongs(id)) background = true;
  }
  for (const shell of shells.data) {
    if (shell.status === "running" && typeof shell.metadata?.sessionID === "string"
        && await belongs(shell.metadata.sessionID)) background = true;
  }
  if (active[rootID]?.type === "running") return "working";
  if (background) return "background";
  return root.outcome === "succeeded" && Number.isFinite(root.time?.idle) ? "ready" : "unknown";
}

export function observeReadiness(context, options, state) {
  let disposed = false;
  let pending = false;
  const poll = async () => {
    if (pending || !state.bound) return;
    pending = true;
    const nativeID = state.bound;
    let readiness = "unknown";
    try { readiness = await snapshot(context.client, nativeID); } catch (_) {}
    if (!disposed) {
      const temporary = `${options.readinessPath}.${randomUUID()}`;
      try {
        writeFileSync(temporary, JSON.stringify({ nativeID, readiness, wrapperPID: options.wrapperPID, observedAt: Date.now() }), { mode: 0o600, flag: "wx" });
        renameSync(temporary, options.readinessPath);
      } catch (_) {
        // The host rejects old evidence if the bridge cannot refresh it.
      } finally {
        try { rmSync(temporary, { force: true }); } catch (_) {}
      }
    }
    pending = false;
  };
  const timer = setInterval(poll, 1000);
  poll();
  return () => { disposed = true; clearInterval(timer); };
}
