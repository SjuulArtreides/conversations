import { useEffect } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, ApiError } from "./api";
import type { AuthConfig } from "./types";
import { refreshLocalHost, useLocalHost } from "./localHost";

/** Query key for the auth-config probe; invalidate it after login/logout. */
export const AUTH_QUERY_KEY = ["auth-config"] as const;

/**
 * Read the SPA's auth state from `/api/auth/config`.
 *
 * `data === null` ⇒ auth is disabled server-side (no login UI anywhere).
 * `data.user === null` ⇒ auth is on but nobody is logged in (gate to /login).
 */
export function useAuth() {
  const host = useLocalHost();
  const query = useQuery<AuthConfig | null>({
    queryKey: AUTH_QUERY_KEY,
    queryFn: api.authConfig,
    enabled: !host.local,
    staleTime: 60_000,
    retry: false,
  });

  const config = host.local ? null : (query.data ?? null);
  return {
    isLoading: !host.local && query.isLoading,
    /** Whether auth is enabled server-side. */
    authEnabled: config !== null,
    /** The logged-in user, or null. */
    user: config?.user ?? null,
    isAdmin: config?.user?.is_admin ?? false,
    googleEnabled: config?.google_enabled ?? false,
    passwordEnabled: config?.password_enabled ?? false,
  };
}

/**
 * When a data query fails with a 401 (e.g. the session expired mid-use),
 * re-probe auth state. That flips `user` to null and the app-level guard then
 * redirects to /login. No-op for any other error.
 */
export function useRedirectOnUnauthorized(error: unknown) {
  const host = useLocalHost();
  const queryClient = useQueryClient();
  useEffect(() => {
    if (error instanceof ApiError && error.status === 401) {
      if (host.local) void refreshLocalHost();
      else void queryClient.invalidateQueries({ queryKey: AUTH_QUERY_KEY });
    }
  }, [error, queryClient, host.local]);
}
