import * as React from "react";
import { api } from "./api";
import { hostControl } from "./hostControl";
import { readPrivatePrCache } from "./privatePrCache";
import type { PrComment, PrReplyRequest, SessionPr } from "./types";

export interface SessionPrSource {
  /** Host cache refresh cadence; remote lists are updated by their existing events. */
  refetchInterval?: number;
  key(base: readonly unknown[]): readonly unknown[];
  listSessionPrs(id: string): Promise<SessionPr[]>;
  listPrComments(id: string, since?: number): Promise<PrComment[]>;
  listPrCommentPreviews(id: string): Promise<PrComment[]>;
  syncPrs(id: string): Promise<{ delivered: boolean }>;
  unlinkSessionPr(
    id: string,
    owner: string,
    repo: string,
    number: number,
  ): Promise<void>;
  replyToPr(id: string, body: PrReplyRequest): Promise<{ delivered: boolean }>;
  sendReply(id: string, text: string): Promise<void>;
}
const remote: SessionPrSource = {
  key: (base) => base,
  listSessionPrs: (id) => api.listSessionPrs(id),
  listPrComments: (id, since) =>
    since === undefined
      ? api.listPrComments(id)
      : api.listPrComments(id, since),
  listPrCommentPreviews: (id) => api.listPrCommentPreviews(id),
  syncPrs: (id) => api.syncPrs(id),
  unlinkSessionPr: (id, owner, repo, number) =>
    api.unlinkSessionPr(id, owner, repo, number),
  replyToPr: (id, body) => api.replyToPr(id, body),
  sendReply: (id, text) => hostControl.sendReply(id, text),
};
export function privateSessionPrSource(hostId: string): SessionPrSource {
  return {
    refetchInterval: 30_000,
    key: (base) => ["private", hostId, ...base],
    listSessionPrs: async (id) => (await readPrivatePrCache(hostId, id)).prs,
    listPrComments: async (id) =>
      (await readPrivatePrCache(hostId, id)).comments,
    listPrCommentPreviews: async (id) =>
      (await readPrivatePrCache(hostId, id)).comments,
    syncPrs: async (id) => {
      for (const pr of (await readPrivatePrCache(hostId, id)).prs)
        await hostControl.syncPr(id, pr, hostId);
      return { delivered: true };
    },
    unlinkSessionPr: async (id, owner, repo, number) => {
      const result = await hostControl.privateRead(
        hostId,
        (channel) =>
          channel.operation("pr.unlink", id, {
            session: id,
            owner,
            repo,
            number,
          }),
        { allowUnlock: true },
      );
      if (result.status !== "executed" && result.status !== "replayed")
        throw new Error(result.detail ?? "The host could not unlink this PR.");
    },
    replyToPr: async (id, body) => {
      await hostControl.commentOnPr(id, body, hostId);
      return { delivered: true };
    },
    sendReply: (id, text) => hostControl.sendReply(id, text, hostId),
  };
}
const Context = React.createContext<SessionPrSource>(remote);
export function SessionPrProvider({
  hostId,
  children,
}: {
  hostId?: string;
  children: React.ReactNode;
}) {
  const source = React.useMemo(
    () => (hostId ? privateSessionPrSource(hostId) : remote),
    [hostId],
  );
  return <Context.Provider value={source}>{children}</Context.Provider>;
}
export const useSessionPrSource = () => React.useContext(Context);
