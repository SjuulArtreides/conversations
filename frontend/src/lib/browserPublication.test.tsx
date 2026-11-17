import "fake-indexeddb/auto";
import { createElement, type PropsWithChildren } from "react";
import { act, renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useBrowserPublication } from "./browserPublication";
import {
  fromBase64Url,
  loadBrowserKey,
  toBase64,
  toBase64Url,
  type BrowserKey,
} from "./hostAuth";
import type { KeyDirectoryUser, PublishedBrowserKey } from "./api";

const state = vi.hoisted(() => ({
  email: "alice@example.com" as string | null,
}));
const api = vi.hoisted(() => ({
  userKeyDirectory: vi.fn(),
  browserKeyChallenge: vi.fn(),
  publishBrowserKey: vi.fn(),
}));
vi.mock("./api", () => ({ api }));
vi.mock("./useAuth", () => ({
  useAuth: () => ({ user: state.email ? { email: state.email } : null }),
}));
let key: BrowserKey;
let client: QueryClient;
let directory: KeyDirectoryUser[];
let payload: string;
function published(
  overrides: Partial<PublishedBrowserKey> = {},
): PublishedBrowserKey {
  return {
    kid: key.kid,
    alg: "es256",
    public_key: toBase64(key.publicRaw),
    label: "Browser",
    created_at: "2026-11-10",
    ...overrides,
  };
}
function renderPublication() {
  return renderHook(() => useBrowserPublication(), {
    wrapper: ({ children }: PropsWithChildren) =>
      createElement(QueryClientProvider, { client }, children),
  });
}
beforeEach(async () => {
  vi.clearAllMocks();
  state.email = "alice@example.com";
  key = await loadBrowserKey();
  client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  directory = [];
  api.userKeyDirectory.mockImplementation(async () => directory);
  api.browserKeyChallenge.mockImplementation(
    async (public_key: string, label: string) => {
      payload = JSON.stringify({
        v: 1,
        purpose: "publish_browser_key",
        user_email: state.email,
        public_key,
        kid: key.kid,
        label: label.trim(),
        challenge: "a".repeat(32),
        expires_at: Math.floor(Date.now() / 1000) + 300,
      });
      return { challenge: "a".repeat(32), payload };
    },
  );
  api.publishBrowserKey.mockResolvedValue({ key: published() });
});
afterEach(() => client.clear());
describe("browser publication", () => {
  it("publishes only on request, proves possession, and shares registration", async () => {
    const { result } = renderPublication();
    const other = renderPublication();
    await waitFor(() => expect(result.current.registration.data).toBe(false));
    expect(api.browserKeyChallenge).not.toHaveBeenCalled();
    expect(api.publishBrowserKey).not.toHaveBeenCalled();
    await act(async () => result.current.publish.mutateAsync("Work browser"));
    await waitFor(() =>
      expect(other.result.current.registration.data).toBe(true),
    );
    expect(result.current.registration.data).toBe(true);
    const [challenge, signature] = api.publishBrowserKey.mock.calls[0];
    expect(challenge).toBe("a".repeat(32));
    expect(api.browserKeyChallenge).toHaveBeenCalledWith(
      toBase64(key.publicRaw),
      "Work browser",
    );
    const publicKey = await crypto.subtle.importKey(
      "raw",
      key.publicRaw,
      { name: "ECDSA", namedCurve: "P-256" },
      true,
      ["verify"],
    );
    expect(
      await crypto.subtle.verify(
        { name: "ECDSA", hash: "SHA-256" },
        publicKey,
        fromBase64Url(signature),
        new TextEncoder().encode("conversations-browser-key/v1\n" + payload),
      ),
    ).toBe(true);
  });
  it.each([
    ["another account", "bob@example.com", {}],
    ["another key", "alice@example.com", { kid: "other-key" }],
    [
      "mismatched public bytes",
      "alice@example.com",
      { public_key: "other-public-key" },
    ],
    ["another algorithm", "alice@example.com", { alg: "ed25519" }],
  ])(
    "does not restore registration from %s",
    async (_case, email, overrides) => {
      directory = [
        {
          email: email as string,
          display_name: null,
          keys: [published(overrides as Partial<PublishedBrowserKey>)],
        },
      ];
      const { result } = renderPublication();
      await waitFor(() => expect(result.current.registration.data).toBe(false));
      expect(api.publishBrowserKey).not.toHaveBeenCalled();
    },
  );
  it("restores the exact account/key and isolates account switching", async () => {
    directory = [
      { email: "alice@example.com", display_name: null, keys: [published()] },
    ];
    const { result, rerender } = renderPublication();
    await waitFor(() => expect(result.current.registration.data).toBe(true));
    state.email = "bob@example.com";
    rerender();
    expect(result.current.registration.data).not.toBe(true);
    await waitFor(() => expect(result.current.registration.data).toBe(false));
  });
  it("keeps an in-flight publication attached to its original account", async () => {
    let finish!: () => void;
    api.publishBrowserKey.mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          finish = resolve;
        }),
    );
    const { result, rerender } = renderPublication();
    await waitFor(() => expect(result.current.registration.data).toBe(false));
    let pending!: Promise<void>;
    act(() => {
      pending = result.current.publish.mutateAsync("Original browser");
    });
    await waitFor(() => expect(api.publishBrowserKey).toHaveBeenCalledTimes(1));
    state.email = "bob@example.com";
    rerender();
    await waitFor(() => expect(result.current.registration.data).toBe(false));
    await act(async () => {
      finish();
      await pending;
    });
    expect(result.current.registration.data).toBe(false);
    expect(
      client.getQueryData([
        "browser-publication",
        "alice@example.com",
        key.kid,
      ]),
    ).toBe(true);
  });

  it("isolates a replacement browser key from the old registration", async () => {
    directory = [
      { email: "alice@example.com", display_name: null, keys: [published()] },
    ];
    const { result } = renderPublication();
    await waitFor(() => expect(result.current.registration.data).toBe(true));
    const pair = await crypto.subtle.generateKey(
      { name: "ECDSA", namedCurve: "P-256" },
      false,
      ["sign", "verify"],
    );
    const publicRaw = new Uint8Array(
      await crypto.subtle.exportKey("raw", pair.publicKey),
    );
    const replacement = {
      privateKey: pair.privateKey,
      publicRaw,
      kid: toBase64Url(
        new Uint8Array(await crypto.subtle.digest("SHA-256", publicRaw)),
      ),
    };
    act(() => client.setQueryData(["browser-public-key"], replacement));
    await waitFor(() =>
      expect(result.current.key.data?.kid).toBe(replacement.kid),
    );
    expect(result.current.registration.data).not.toBe(true);
    await waitFor(() => expect(result.current.registration.data).toBe(false));
  });
  it.each(["challenge", "proof", "publication"])(
    "keeps %s failures retryable without reporting publication",
    async (step) => {
      if (step === "challenge")
        api.browserKeyChallenge.mockRejectedValue(
          new Error("challenge failed"),
        );
      if (step === "proof")
        api.browserKeyChallenge.mockResolvedValue({
          challenge: "a".repeat(32),
          payload: JSON.stringify({ purpose: "wrong" }),
        });
      if (step === "publication")
        api.publishBrowserKey.mockRejectedValue(
          new Error("publication failed"),
        );
      const { result } = renderPublication();
      await waitFor(() => expect(result.current.registration.data).toBe(false));
      await act(async () => {
        await expect(
          result.current.publish.mutateAsync("Browser"),
        ).rejects.toThrow();
      });
      expect(result.current.registration.data).toBe(false);
      await waitFor(() => expect(result.current.publish.isError).toBe(true));
      expect(api.browserKeyChallenge).toHaveBeenCalledTimes(1);
      expect(api.publishBrowserKey).toHaveBeenCalledTimes(
        step === "publication" ? 1 : 0,
      );
    },
  );
  it("keeps directory failure distinct from an unpublished browser", async () => {
    api.userKeyDirectory.mockRejectedValue(new Error("directory unavailable"));
    const { result } = renderPublication();
    await waitFor(() => expect(result.current.registration.isError).toBe(true));
    expect(result.current.registration.data).toBeUndefined();
  });
  it("requires sign-in and never queries a previous account", async () => {
    state.email = null;
    const { result } = renderPublication();
    await waitFor(() => expect(result.current.key.isSuccess).toBe(true));
    await act(async () => {
      await expect(
        result.current.publish.mutateAsync("Browser"),
      ).rejects.toThrow("Sign in");
    });
    expect(api.userKeyDirectory).not.toHaveBeenCalled();
    expect(api.browserKeyChallenge).not.toHaveBeenCalled();
  });
});
