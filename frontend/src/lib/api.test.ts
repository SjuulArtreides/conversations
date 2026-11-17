import { afterEach, describe, expect, it, vi } from "vitest";
import { api, buildSessionsQuery, buildUsersQuery } from "./api";

describe("query builders", () => {
  it("builds the sessions query from search + project filter", () => {
    expect(buildSessionsQuery({})).toBe("/api/sessions");
    expect(buildSessionsQuery({ q: "  terraform " })).toBe(
      "/api/sessions?q=terraform",
    );
    expect(buildSessionsQuery({ project: "/Users/dev/proj" })).toBe(
      "/api/sessions?project=%2FUsers%2Fdev%2Fproj",
    );
    const both = buildSessionsQuery({ q: "deploy", project: "p" });
    expect(both).toContain("q=deploy");
    expect(both).toContain("project=p");
  });

  it("ignores blank/whitespace-only search", () => {
    expect(buildSessionsQuery({ q: "   " })).toBe("/api/sessions");
  });

  it("builds the users query with limit", () => {
    expect(buildUsersQuery("ali", 5)).toBe("/api/users?q=ali&limit=5");
    expect(buildUsersQuery("")).toBe("/api/users");
  });
});

describe("api client", () => {
  afterEach(() => vi.restoreAllMocks());

  it("appInfo GETs /api/app", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ version: "0.8.1" }),
    });
    vi.stubGlobal("fetch", fetchMock);

    const info = await api.appInfo();
    expect(fetchMock.mock.calls[0][0]).toBe("/api/app");
    expect(info).toEqual({ version: "0.8.1" });
  });

  it("searchUsers calls /api/users and unwraps the users array", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: () =>
        Promise.resolve({ users: [{ email: "a@x.com", display_name: "A" }] }),
    });
    vi.stubGlobal("fetch", fetchMock);

    const users = await api.searchUsers("a", 8);
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/users?q=a&limit=8",
      expect.anything(),
    );
    expect(users).toEqual([{ email: "a@x.com", display_name: "A" }]);
  });

  it("setSharing PUTs the payload as JSON", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: () =>
        Promise.resolve({ visibility: "restricted", emails: ["a@x.com"] }),
    });
    vi.stubGlobal("fetch", fetchMock);

    const res = await api.setSharing("sess-1", {
      visibility: "restricted",
      emails: ["a@x.com"],
    });
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("/api/sessions/sess-1/sharing");
    expect(init.method).toBe("PUT");
    expect(JSON.parse(init.body)).toEqual({
      visibility: "restricted",
      emails: ["a@x.com"],
    });
    expect(res.visibility).toBe("restricted");
  });

  it("throws ApiError with the server error detail on non-2xx", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 404,
        statusText: "Not Found",
        json: () => Promise.resolve({ error: "not found" }),
      }),
    );
    await expect(api.getSession("nope")).rejects.toMatchObject({
      status: 404,
      message: "not found",
    });
  });

  it("authConfig resolves to null when auth is disabled (404)", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 404,
        statusText: "Not Found",
        json: () => Promise.resolve({ error: "auth disabled" }),
      }),
    );
    await expect(api.authConfig()).resolves.toBeNull();
  });

  it("authConfig returns the config when auth is enabled", async () => {
    const cfg = {
      google_enabled: false,
      password_enabled: true,
      user: null,
    };
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        status: 200,
        headers: { get: () => "application/json" },
        json: () => Promise.resolve(cfg),
      }),
    );
    await expect(api.authConfig()).resolves.toEqual(cfg);
  });

  it("logout rejects a non-2xx response", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: false,
      status: 500,
      statusText: "Internal Server Error",
      json: () => Promise.resolve({ error: "Could not clear the session" }),
    });
    vi.stubGlobal("fetch", fetchMock);

    await expect(api.logout()).rejects.toMatchObject({
      status: 500,
      message: "Could not clear the session",
    });
    expect(fetchMock).toHaveBeenCalledWith("/auth/logout", {
      method: "POST",
      headers: { Accept: "application/json" },
    });
  });

  it("register POSTs email + password as JSON", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: () =>
        Promise.resolve({ email: "dev@example.com", display_name: "Dev" }),
    });
    vi.stubGlobal("fetch", fetchMock);

    await api.register({ email: "dev@example.com", password: "hunter2pass" });
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("/auth/register");
    expect(init.method).toBe("POST");
    expect(JSON.parse(init.body)).toEqual({
      email: "dev@example.com",
      password: "hunter2pass",
    });
  });

  it("completeLive POSTs to the complete endpoint", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: () => Promise.resolve(undefined),
    });
    vi.stubGlobal("fetch", fetchMock);

    await api.completeLive("sess-1");
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("/api/live/sess-1/complete");
    expect(init.method).toBe("POST");
  });

  it("getUserSettings GETs /api/user/settings and parses container_paths", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ container_paths: ["/Users/dev/mono"] }),
    });
    vi.stubGlobal("fetch", fetchMock);

    const res = await api.getUserSettings();
    expect(fetchMock.mock.calls[0][0]).toBe("/api/user/settings");
    expect(res).toEqual({ container_paths: ["/Users/dev/mono"] });
  });

  it("setUserSettings PUTs paths and default agent as JSON and parses the echo", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: () =>
        Promise.resolve({ container_paths: ["/Users/dev/mono", "/srv/repo"] }),
    });
    vi.stubGlobal("fetch", fetchMock);

    const res = await api.setUserSettings(["/Users/dev/mono", "/srv/repo"]);
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("/api/user/settings");
    expect(init.method).toBe("PUT");
    expect(init.headers["Content-Type"]).toBe("application/json");
    expect(JSON.parse(init.body)).toEqual({
      container_paths: ["/Users/dev/mono", "/srv/repo"],
      default_agent: "claude",
    });
    expect(res).toEqual({
      container_paths: ["/Users/dev/mono", "/srv/repo"],
    });
  });

  it("setUserSettings includes inline_code_colors in the body when provided", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ container_paths: [] }),
    });
    vi.stubGlobal("fetch", fetchMock);

    await api.setUserSettings([], "claude", { palette: [], rgb: [0xb1b9f9] });
    const body = JSON.parse(fetchMock.mock.calls[0][1].body);
    expect(body).toEqual({
      container_paths: [],
      default_agent: "claude",
      inline_code_colors: { palette: [], rgb: [0xb1b9f9] },
    });
  });

  it("setUserSettings sends account preferences only, never launch flags", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ container_paths: [] }),
    });
    vi.stubGlobal("fetch", fetchMock);

    await api.setUserSettings([], "codex");
    const body = JSON.parse(fetchMock.mock.calls[0][1].body);
    expect(body).toEqual({ container_paths: [], default_agent: "codex" });
  });

  it("setUserSettings omits inline_code_colors when not provided", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ container_paths: [] }),
    });
    vi.stubGlobal("fetch", fetchMock);

    await api.setUserSettings([], "claude");
    const body = JSON.parse(fetchMock.mock.calls[0][1].body);
    expect("inline_code_colors" in body).toBe(false);
  });
});

describe("shared live order", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });
  it("rebases an explicit gesture after CAS conflict and preserves newly registered sessions", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({ revision: 3, session_ids: ["a", "b", "c"] }),
        ),
      )
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ error: "changed" }), { status: 409 }),
      )
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({ revision: 4, session_ids: ["a", "new", "b", "c"] }),
        ),
      )
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({ revision: 5, session_ids: ["c", "a", "new", "b"] }),
        ),
      );
    vi.stubGlobal("fetch", fetchMock);
    const result = await api.reorderLiveSession("c", "a");
    expect(result.session_ids).toEqual(["c", "a", "new", "b"]);
    expect(
      new Headers(fetchMock.mock.calls[3][1].headers).get("Content-Type"),
    ).toBe("application/json");
    expect(JSON.parse(fetchMock.mock.calls[3][1].body)).toEqual({
      revision: 4,
      session_ids: ["c", "a", "new", "b"],
    });
  });
  it("does not retry a failed storage request", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ revision: 3, session_ids: ["a", "b"] })),
      )
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ error: "save unavailable" }), {
          status: 503,
        }),
      );
    vi.stubGlobal("fetch", fetchMock);
    await expect(api.reorderLiveSession("b", "a")).rejects.toThrow(
      "save unavailable",
    );
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });
});

describe("browser key publication", () => {
  afterEach(() => vi.unstubAllGlobals());

  it.each(["challenge", "publication"])(
    "sends the %s request as JSON",
    async (step) => {
      const response =
        step === "challenge"
          ? { challenge: "nonce", payload: "signed-statement" }
          : { key: { kid: "fingerprint", label: "Browser" } };
      const fetchMock = vi.fn(async (url: string, init: RequestInit) => {
        const request = new Request(new URL(url, "https://example.test"), init);
        if (request.headers.get("Content-Type") !== "application/json") {
          return new Response("Expected JSON", {
            status: 415,
            statusText: "Unsupported Media Type",
          });
        }
        return Response.json(response);
      });
      vi.stubGlobal("fetch", fetchMock);
      const result =
        step === "challenge"
          ? await api.browserKeyChallenge("public-key", "Browser")
          : await api.publishBrowserKey("nonce", "signature");
      expect(result).toEqual(response);
      const [url, init] = fetchMock.mock.calls[0];
      expect(url).toBe(
        step === "challenge" ? "/api/user-keys/challenge" : "/api/user-keys",
      );
      expect(init.method).toBe("POST");
      expect(JSON.parse(init.body as string)).toEqual(
        step === "challenge"
          ? { public_key: "public-key", label: "Browser" }
          : { challenge: "nonce", signature: "signature" },
      );
    },
  );
});
