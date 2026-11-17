import { useQuery } from "@tanstack/react-query";

/** Survives disposal of the button and terminal that initiated the restart. */
export interface SessionRestart {
  pending: boolean;
  generation: number;
}

export const sessionRestartKey = (id: string) =>
  ["session-restart", id] as const;
export const INITIAL_SESSION_RESTART: SessionRestart = {
  pending: false,
  generation: 0,
};

export function useSessionRestart(id: string): SessionRestart {
  const { data } = useQuery<SessionRestart>({
    queryKey: sessionRestartKey(id),
    enabled: false,
    initialData: INITIAL_SESSION_RESTART,
    staleTime: Infinity,
  });
  return data;
}
