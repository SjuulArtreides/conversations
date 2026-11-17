import { createRoot, createEffect } from "solid-js";
import { writeFileSync, renameSync, rmSync } from "node:fs";
import { randomUUID } from "node:crypto";

// Identity is latched from this terminal's root route, never from server events.
export function observe(route, session, options, state = {}) {
  let warned = false;
  return createRoot(dispose => {
    createEffect(() => {
      const current = route();
      if (!state.bound) {
        if (!current || (options.knownID && current !== options.knownID)) return;
        const info = session(current);
        if (!info || info.id !== current || info.parentID) return;
        state.bound = current;
      }
      const info = session(state.bound);
      if (!info || info.id !== state.bound) return;
      // Exact compatibility fallbacks from v1.18.34 session/session.ts and
      // v2.0.27 util/session-title-fallback.ts; ordinary named sessions publish immediately.
      const fallback = /^(New session|Child session) - \d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/;
      const report = JSON.stringify({
        launchID: options.launchID,
        nativeID: state.bound,
        title: typeof info.title === "string" && !fallback.test(info.title) ? info.title : null,
      });
      if (report === state.previous) return;
      const temporary = `${options.reportPath}.${randomUUID()}`;
      try {
        writeFileSync(temporary, report, { mode: 0o600, flag: "wx" });
        renameSync(temporary, options.reportPath);
        state.previous = report;
      } catch (_) {
        if (!warned) console.error("CONVERSATIONS_BRIDGE_WRITE: Cannot update the session name. Check the host runtime directory permissions.");
        warned = true;
      } finally {
        try { rmSync(temporary, { force: true }); } catch (_) {}
      }
    });
    return dispose;
  });
}
