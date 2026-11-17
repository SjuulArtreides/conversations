import { useQuery } from "@tanstack/react-query";
import { api } from "./api";
import type { LlmStatus } from "./types";

/** Query key for the LLM-status probe. */
export const LLM_STATUS_QUERY_KEY = ["llm-status"] as const;

/**
 * Read whether server-side LLM features are enabled from `/api/llm/status`.
 *
 * That route is always mounted and always returns JSON, so (unlike the
 * auth-config probe) there's no null/404 handling to do.
 */
export function useLlmStatus() {
  const query = useQuery<LlmStatus>({
    queryKey: LLM_STATUS_QUERY_KEY,
    queryFn: api.llmStatus,
    staleTime: 300_000,
    retry: false,
  });
  return {
    enabled: query.data?.enabled ?? false,
    model: query.data?.model ?? null,
    isLoading: query.isLoading,
  };
}
