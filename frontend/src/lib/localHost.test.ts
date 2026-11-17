import { afterEach, expect, it, vi } from "vitest";
afterEach(() => {
  vi.unstubAllGlobals();
  vi.resetModules();
  history.replaceState(null, "", "/");
});
it("consumes bootstrap only on loopback, clears it first, and sends no private key", async () => {
  history.replaceState(null, "", "/#token=owner-token");
  const fetcher = vi.fn(async (path: string, init?: RequestInit) => {
    if (path === "/api/local")
      return new Response(
        JSON.stringify({ local: true, authenticated: false }),
        { headers: { "content-type": "application/json" } },
      );
    expect(location.hash).toBe("");
    expect(path).toBe("/api/unlock");
    expect(JSON.parse(init!.body as string)).toEqual({ token: "owner-token" });
    return new Response("{}", {
      headers: { "content-type": "application/json" },
    });
  });
  vi.stubGlobal("fetch", fetcher);
  const local = await import("./localHost");
  await local.initializeLocalHost();
  expect(local.localHostState()).toMatchObject({
    local: true,
    authenticated: true,
    error: null,
  });
  expect(location.hash).toBe("");
});
it("fails closed when owner cookie is refused", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(
      async () =>
        new Response(JSON.stringify({ local: true, authenticated: true }), {
          headers: { "content-type": "application/json" },
        }),
    ),
  );
  const local = await import("./localHost");
  await local.initializeLocalHost();
  vi.stubGlobal(
    "fetch",
    vi.fn(
      async () =>
        new Response(JSON.stringify({ error: "Owner access required" }), {
          status: 401,
          headers: { "content-type": "application/json" },
        }),
    ),
  );
  await expect(local.localRequest("/api/local/sessions")).rejects.toThrow(
    "Owner access required",
  );
  expect(local.localHostState().authenticated).toBe(false);
});

it("preserves a session message anchor and never sends it to unlock", async () => {
  history.replaceState(null, "", "/private/session#m-message");
  const fetcher = vi.fn(
    async () =>
      new Response(JSON.stringify({ local: true, authenticated: true }), {
        headers: { "content-type": "application/json" },
      }),
  );
  vi.stubGlobal("fetch", fetcher);
  const local = await import("./localHost");
  await local.initializeLocalHost();
  expect(location.hash).toBe("#m-message");
  expect(fetcher).toHaveBeenCalledTimes(1);
  expect(fetcher).toHaveBeenCalledWith("/api/local", expect.anything());
});

it("keeps valid owner access when a restored setup link is stale", async () => {
  history.replaceState(null, "", "/#token=spent-token");
  const fetcher = vi.fn(async (path: string) =>
    path === "/api/local"
      ? new Response(JSON.stringify({ local: true, authenticated: true }), {
          headers: { "content-type": "application/json" },
        })
      : new Response(JSON.stringify({ error: "Link already used" }), {
          status: 401,
          headers: { "content-type": "application/json" },
        }),
  );
  vi.stubGlobal("fetch", fetcher);
  const local = await import("./localHost");
  await local.initializeLocalHost();
  expect(local.localHostState()).toMatchObject({
    authenticated: true,
    error: null,
  });
  expect(location.hash).toBe("");
  expect(fetcher).toHaveBeenCalledTimes(1);
});
