import { hostControl } from "./hostControl";
import { listSessions } from "./privateChannel";

export const privateTerminalGenerationKey = (hostId: string, session: string) =>
  ["private-terminal-generation", hostId, session] as const;

/** Never reopen until the host confirms the old terminal has stopped. */
export async function restartPrivateSession(hostId: string, session: string) {
  async function waitFor(running: boolean) {
    const deadline = Date.now() + 15_000;
    while (true) {
      const active = await hostControl.privateRead(hostId, (channel) =>
        listSessions(channel, { runningOnly: true }),
      );
      if (
        active.some((entry) => entry.id === session && entry.running) ===
        running
      )
        return;
      if (Date.now() >= deadline)
        throw new Error(
          `The host has not confirmed the session ${running ? "restarted" : "stopped"}. Check the host before trying again.`,
        );
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
  }
  await hostControl.endSession(session, hostId);
  await waitFor(false);
  await hostControl.reopenSession(session, hostId);
  await waitFor(true);
}
