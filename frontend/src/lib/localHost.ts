import * as React from "react";
export const LOCAL_HOST_ID = "local";
export interface LocalHostState {
  local: boolean;
  authenticated: boolean;
  error: string | null;
}
let state: LocalHostState = { local: false, authenticated: false, error: null };
const listeners = new Set<() => void>();
function publish(next: LocalHostState) {
  state = next;
  listeners.forEach((listener) => listener());
}
export const localHostState = () => state;
export const isLocalHost = () => state.local;
export function useLocalHost() {
  return React.useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    localHostState,
    localHostState,
  );
}
const loopback = () =>
  ["localhost", "127.0.0.1", "[::1]"].includes(window.location.hostname);

/** Discover transport before rendering. The fragment never reaches a server URL. */
export async function initializeLocalHost(): Promise<void> {
  if (!loopback()) return;
  const token = window.location.hash.startsWith("#token=")
    ? (new URLSearchParams(window.location.hash.slice(1)).get("token") ?? "")
    : "";
  if (token)
    history.replaceState(
      history.state,
      "",
      window.location.pathname + window.location.search,
    );
  try {
    const response = await fetch("/api/local", {
      credentials: "same-origin",
      redirect: "error",
    });
    if (
      !response.ok ||
      !response.headers.get("content-type")?.includes("application/json")
    )
      return;
    const value = (await response.json()) as {
      local?: boolean;
      authenticated?: boolean;
    };
    if (value.local !== true) return;
    publish({
      local: true,
      authenticated: value.authenticated === true,
      error: null,
    });
    if (token && value.authenticated !== true) {
      await localRequest("/api/unlock", {
        method: "POST",
        body: JSON.stringify({ token }),
      });
      publish({ local: true, authenticated: true, error: null });
    }
  } catch (error) {
    publish({
      ...state,
      error:
        error instanceof Error
          ? error.message
          : "Local host could not be reached.",
    });
  }
}
export async function refreshLocalHost(): Promise<void> {
  if (!state.local) return;
  try {
    const value = await localRequest<{
      local: boolean;
      authenticated: boolean;
    }>("/api/local");
    publish({
      local: true,
      authenticated: value.authenticated === true,
      error: null,
    });
  } catch (error) {
    publish({
      ...state,
      error:
        error instanceof Error
          ? error.message
          : "Local host could not be reached.",
    });
  }
}
/** Owner-cookie transport only; never redirect private requests to another server. */
export async function localRequest<T>(
  path: string,
  init: RequestInit = {},
): Promise<T> {
  if (!state.local || !loopback())
    throw new Error("This action requires the local host application.");
  if (!path.startsWith("/api/") || path.startsWith("//"))
    throw new Error("Invalid local endpoint.");
  const headers = new Headers(init.headers);
  if (init.body) headers.set("Content-Type", "application/json");
  const response = await fetch(path, {
    ...init,
    headers,
    credentials: "same-origin",
    redirect: "error",
  });
  const body: unknown = await response.json().catch(() => null);
  if (!response.ok) {
    const detail =
      body && typeof body === "object" && "error" in body
        ? String(body.error)
        : `Local host refused the request (${response.status}).`;
    if (response.status === 401)
      publish({ ...state, authenticated: false, error: detail });
    throw new Error(detail);
  }
  return body as T;
}
