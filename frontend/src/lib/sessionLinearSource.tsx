import * as React from "react";
import { api } from "./api";
import { hostControl } from "./hostControl";
import type { SessionLinearDoc } from "./types";

export interface SessionLinearSource {
  key(base: readonly unknown[]): readonly unknown[];
  list(id: string): Promise<SessionLinearDoc[]>;
  unlink(id: string, url: string): Promise<void>;
}
const remote: SessionLinearSource = {
  key: (base) => base,
  list: (id) => api.listSessionLinearDocs(id),
  unlink: (id, url) => api.unlinkSessionLinearDoc(id, url),
};
export function privateSessionLinearSource(
  hostId: string,
): SessionLinearSource {
  return {
    key: (base) => ["private", hostId, ...base],
    list: (id) =>
      hostControl.privateRead(hostId, async (channel) => {
        const docs: SessionLinearDoc[] = [];
        const cursors = new Set<string>();
        let after: string | null = null;
        do {
          const result = await channel.operation("linear.list", id, {
            session: id,
            after,
            limit: 32,
          });
          if (!result.body || result.body.kind !== "linear")
            throw new Error(
              result.detail ?? "The host could not read Linear references.",
            );
          docs.push(...result.body.docs);
          after = result.body.next;
          if (after !== null) {
            if (cursors.has(after))
              throw new Error("The host repeated a Linear reference page.");
            cursors.add(after);
          }
        } while (after !== null);
        return docs;
      }),
    unlink: async (id, url) => {
      const result = await hostControl.privateRead(
        hostId,
        (channel) =>
          channel.operation("linear.unlink", id, { session: id, url }),
        { allowUnlock: true },
      );
      if (result.status !== "executed" && result.status !== "replayed")
        throw new Error(
          result.detail ?? "The host could not unlink this reference.",
        );
    },
  };
}
const Context = React.createContext(remote);
export function SessionLinearProvider({
  hostId,
  children,
}: {
  hostId?: string;
  children: React.ReactNode;
}) {
  const source = React.useMemo(
    () => (hostId ? privateSessionLinearSource(hostId) : remote),
    [hostId],
  );
  return <Context.Provider value={source}>{children}</Context.Provider>;
}
export const useSessionLinearSource = () => React.useContext(Context);
