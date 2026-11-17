export interface PublishedBrowserKey {
  kid: string;
  alg: "es256";
  public_key: string;
  label: string;
  created_at: string;
}
export interface KeyDirectoryUser {
  email: string;
  display_name: string | null;
  keys: PublishedBrowserKey[];
}

import { hostControl } from "./hostControl";
import type { LiveOrder } from "./types";
import type {
  SessionNotification,
  AttentionRecipient,
  AppInfo,
  AuthConfig,
  AgentKind,
  AdminDirectoryUser,
  AdminUser,
  Chat,
  ChatWithMessages,
  CurrentUser,
  LiveSession,
  LiveShare,
  LlmModel,
  LlmStatus,
  PasswordResetLink,
  PrComment,
  PrReplyRequest,
  PrReplyResult,
  PrSyncResult,
  Project,
  InlineCodeColors,
  ProjectDetail,
  ProjectInfo,
  RepoLink,
  RepoLinkInput,
  Session,
  SessionLinearDoc,
  SessionCommunications,
  SessionPr,
  SessionSummary,
  SharingPayload,
  Summary,
  UserInfo,
  UserSettings,
} from "./types";

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

export async function errorFromResponse(res: Response): Promise<ApiError> {
  let detail = res.statusText;
  try {
    const body = await res.json();
    detail = body?.error ?? body?.message ?? detail;
  } catch {
    /* ignore non-json bodies */
  }
  return new ApiError(res.status, detail);
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    headers: { Accept: "application/json", ...(init?.headers ?? {}) },
    ...init,
  });
  if (!res.ok) {
    throw await errorFromResponse(res);
  }
  return (await res.json()) as T;
}

async function del(url: string, method = "DELETE"): Promise<void> {
  const res = await fetch(url, {
    method,
    headers: { Accept: "application/json" },
  });
  if (!res.ok) throw await errorFromResponse(res);
}

/** Path-segment encoder (alias for `encodeURIComponent`, used for ids). */
export const enc = encodeURIComponent;

/** Build the `/api/sessions` query string from search + project filter. */
export function buildSessionsQuery(params: {
  q?: string;
  project?: string;
}): string {
  const sp = new URLSearchParams();
  if (params.q && params.q.trim()) sp.set("q", params.q.trim());
  if (params.project && params.project.trim())
    sp.set("project", params.project);
  const qs = sp.toString();
  return qs ? `/api/sessions?${qs}` : "/api/sessions";
}

export function buildUsersQuery(q: string, limit?: number): string {
  const sp = new URLSearchParams();
  if (q && q.trim()) sp.set("q", q.trim());
  if (limit != null) sp.set("limit", String(limit));
  const qs = sp.toString();
  return qs ? `/api/users?${qs}` : "/api/users";
}

export const api = {
  appInfo: () => request<AppInfo>("/api/app"),

  listSessions: (params: { q?: string; project?: string }) =>
    request<{ sessions: Summary[] }>(buildSessionsQuery(params)).then(
      (r) => r.sessions,
    ),

  getSession: (id: string) =>
    request<Session>(`/api/sessions/${encodeURIComponent(id)}`),

  llmStatus: () => request<LlmStatus>("/api/llm/status"),

  /** Currently-live (streaming-now) sessions, newest first. `id == session_id`. */
  liveSessions: () =>
    request<{ live_sessions: LiveSession[] }>("/api/live").then(
      (r) => r.live_sessions,
    ),

  liveOrder: () => request<LiveOrder>("/api/live/order"),

  async reorderLiveSession(
    id: string,
    beforeId: string | null,
  ): Promise<LiveOrder> {
    // Retry only this explicit gesture, never a background snapshot.
    for (let attempt = 0; attempt < 3; attempt++) {
      const current = await api.liveOrder();
      if (!current.session_ids.includes(id))
        throw new Error("This session is no longer live");
      const ids = current.session_ids.filter((candidate) => candidate !== id);
      const index = beforeId === null ? ids.length : ids.indexOf(beforeId);
      if (index < 0)
        throw new Error("The tab order changed. Please try again.");
      ids.splice(index, 0, id);
      try {
        return await request<LiveOrder>("/api/live/order", {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            revision: current.revision,
            session_ids: ids,
          }),
        });
      } catch (error) {
        if (
          !(error instanceof ApiError) ||
          error.status !== 409 ||
          attempt === 2
        )
          throw error;
      }
    }
    throw new Error("The tab order changed. Please try again.");
  },

  /**
   * Read a session's current person share list (author-only; 403 otherwise).
   */
  getLiveSharing: (id: string) =>
    request<{ shares: LiveShare[] }>(`/api/live/${enc(id)}/sharing`).then(
      (r) => r.shares,
    ),

  /**
   * Replace a session's person share list (author-only). Returns the stored
   * shares.
   */
  setLiveSharing: (id: string, shares: LiveShare[], reason?: string) =>
    request<{ shares: LiveShare[] }>(`/api/live/${enc(id)}/sharing`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ shares, ...(reason ? { reason } : {}) }),
    }).then((r) => r.shares),

  /**
   * Graduate / end a running live session (author-only;
   * `POST /api/live/{id}/complete`). The session stops streaming and becomes an
   * ordinary stored session; the live stream emits its terminal `complete`
   * frame, which the view already reacts to.
   */
  completeLive: (id: string) =>
    request<void>(`/api/live/${enc(id)}/complete`, { method: "POST" }),

  /**
   * Soft-close a live session you can see but do NOT own
   * (`POST /api/live/{id}/dismiss`). Unlike {@link completeLive}, any viewer
   * (author or sharee) may call it: the session graduates to an ordinary stored
   * session and drops out of the live list, but the owner's CLI process is left
   * running. Use it to clear a colleague's stale live session from your list;
   * it stays available in the sessions view afterwards.
   */
  dismissLive: (id: string) =>
    request<{ session_id: string }>(`/api/live/${enc(id)}/dismiss`, {
      method: "POST",
    }),

  generateSummary: (id: string) =>
    request<SessionSummary>(`/api/sessions/${encodeURIComponent(id)}/summary`, {
      method: "POST",
    }),

  /** Delete a session. 403 if you're not the owner, 404 if it's already gone. */
  deleteSession: (id: string) => del(`/api/sessions/${encodeURIComponent(id)}`),

  // Distinct project *paths* with session counts (the SessionList filter).
  // The explicit, user-named projects feature owns `/api/projects`; this
  // path-grouping endpoint moved to `/api/project-paths` (server slice 5a).
  listProjects: () =>
    request<{ projects: ProjectInfo[] }>("/api/project-paths").then(
      (r) => r.projects,
    ),

  // ── Explicit projects (slice 5a) ──────────────────────────────────────
  /** List the explicit, user-named projects. */
  projects: () =>
    request<{ projects: Project[] }>("/api/projects").then((r) => r.projects),

  createProject: (name: string, description?: string) =>
    request<Project>("/api/projects", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name, description }),
    }),

  getProject: (id: string) =>
    request<ProjectDetail>(`/api/projects/${enc(id)}`),

  updateProject: (id: string, patch: { name: string; description?: string }) =>
    request<Project>(`/api/projects/${enc(id)}`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(patch),
    }),

  deleteProject: (id: string) => del(`/api/projects/${enc(id)}`),

  /** Assign (or clear, with `null`) the explicit project of a session. */
  setSessionProject: (id: string, projectId: string | null) =>
    request<{ project_id: string | null }>(`/api/sessions/${enc(id)}/project`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ project_id: projectId }),
    }),

  // ── Chat threads (slice 5a CRUD; streaming send lives in chatStream.ts) ──
  sessionChats: (id: string) =>
    request<{ chats: Chat[] }>(`/api/sessions/${enc(id)}/chats`).then(
      (r) => r.chats,
    ),

  createSessionChat: (id: string, title?: string) =>
    request<Chat>(`/api/sessions/${enc(id)}/chats`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(title != null ? { title } : {}),
    }),

  projectChats: (id: string) =>
    request<{ chats: Chat[] }>(`/api/projects/${enc(id)}/chats`).then(
      (r) => r.chats,
    ),

  createProjectChat: (id: string, title?: string) =>
    request<Chat>(`/api/projects/${enc(id)}/chats`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(title != null ? { title } : {}),
    }),

  /** A chat plus its full message history. 404 ⇒ not visible to this user. */
  getChat: (id: string) => request<ChatWithMessages>(`/api/chats/${enc(id)}`),

  updateChat: (id: string, patch: { shared: boolean; title?: string }) =>
    request<Chat>(`/api/chats/${enc(id)}`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(patch),
    }),

  deleteChat: (id: string) => del(`/api/chats/${enc(id)}`),

  userKeyDirectory: (q: string, limit = 20) =>
    request<{ users: KeyDirectoryUser[] }>(
      `/api/user-keys?q=${enc(q)}&limit=${limit}`,
    ).then((r) => r.users),
  browserKeyChallenge: (public_key: string, label: string) =>
    request<{ challenge: string; payload: string }>(
      "/api/user-keys/challenge",
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ public_key, label }),
      },
    ),
  publishBrowserKey: (challenge: string, signature: string) =>
    request<{ key: PublishedBrowserKey }>("/api/user-keys", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ challenge, signature }),
    }),

  searchUsers: (q: string, limit = 8) =>
    request<{ users: UserInfo[] }>(buildUsersQuery(q, limit)).then(
      (r) => r.users,
    ),

  notifications: () => request<SessionNotification[]>("/api/notifications"),

  readNotification: (id: string) =>
    request<unknown>(`/api/notifications/${enc(id)}/read`, { method: "POST" }),

  resolveNotification: (id: string) =>
    request<unknown>(`/api/notifications/${enc(id)}/resolve`, {
      method: "POST",
    }),

  attentionRecipients: (id: string) =>
    request<AttentionRecipient[]>(
      `/api/sessions/${enc(id)}/attention-recipients`,
    ),

  requestAttention: (id: string, recipient_email: string, message: string) =>
    request<{ ok: boolean }>(`/api/sessions/${enc(id)}/attention`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ recipient_email, message }),
    }),

  setSharing: (id: string, payload: SharingPayload) =>
    request<SharingPayload>(`/api/sessions/${encodeURIComponent(id)}/sharing`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    }),

  /**
   * Rename a session via a DB-only title override (the on-disk transcript is
   * never modified). A blank/empty `title` clears the override, reverting to
   * the parsed transcript title. Returns the effective title.
   */
  setTitle: (id: string, title: string) =>
    request<{ title: string }>(
      `/api/sessions/${encodeURIComponent(id)}/title`,
      {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ title }),
      },
    ).then((r) => r.title),

  /**
   * Replace the canonical repo links for a session. Per-user local paths of
   * surviving repos are preserved server-side; the response carries the full
   * stored links (with paths).
   */
  setRepos: (id: string, repos: RepoLinkInput[]) =>
    request<{ repos: RepoLink[] }>(
      `/api/sessions/${encodeURIComponent(id)}/repos`,
      {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ repos }),
      },
    ).then((r) => r.repos),

  // ── Per-user settings (top-level / "container" repos) ─────────────────
  /**
   * The current viewer's settings — the list of absolute "container" repo
   * paths. Returns an empty list when none are set (never errors server-side).
   */
  getUserSettings: () => request<UserSettings>("/api/user/settings"),

  /**
   * Replace the viewer's full set of container repo paths. Paths are
   * normalized server-side (trailing slash trimmed, blanks/dupes dropped); the
   * normalized echo is returned. Requires an authenticated user (401/403
   * otherwise).
   *
   * `inline_code_colors` is wholesale like the rest of this PUT: pass the
   * current value to preserve it; OMIT it (leave `undefined`) only to clear it.
   */
  setUserSettings: (
    container_paths: string[],
    default_agent: AgentKind = "claude",
    inline_code_colors?: InlineCodeColors | null,
  ) =>
    request<UserSettings>("/api/user/settings", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        container_paths,
        default_agent,
        ...(inline_code_colors !== undefined ? { inline_code_colors } : {}),
      }),
    }),

  /**
   * Probe auth state. Resolves to `null` when auth is disabled server-side — in
   * that mode this route isn't mounted, so the request either 404s (API-only)
   * or, more commonly, the SPA static fallback returns the `index.html` shell
   * (200 text/html). Both mean "auth off" → no login UI. Only a real JSON
   * response (auth on) yields an [`AuthConfig`].
   */
  authConfig: async (): Promise<AuthConfig | null> => {
    const res = await fetch("/api/auth/config", {
      headers: { Accept: "application/json" },
    });
    if (res.status === 404) return null;
    const contentType = res.headers.get("content-type") ?? "";
    if (!contentType.includes("application/json")) return null;
    if (!res.ok) throw await errorFromResponse(res);
    return (await res.json()) as AuthConfig;
  },

  register: (body: {
    email: string;
    password: string;
    display_name?: string;
  }) =>
    request<CurrentUser>("/auth/register", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }),

  login: (body: { email: string; password: string }) =>
    request<CurrentUser>("/auth/login", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }),

  createPasswordResetLink: (email: string) =>
    request<PasswordResetLink>("/api/admin/password-reset-links", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ email }),
    }),

  listAdmins: () =>
    request<{ admins: AdminUser[] }>("/api/admin/admins").then(
      (response) => response.admins,
    ),

  listAdminUsers: () =>
    request<{ users: AdminDirectoryUser[] }>("/api/admin/users").then(
      (response) => response.users,
    ),

  grantAdmin: (email: string) =>
    request<AdminUser>("/api/admin/admins", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ email }),
    }),

  revokeAdmin: (email: string) => del(`/api/admin/admins/${enc(email)}`),

  deleteUser: (email: string) => del(`/api/admin/users/${enc(email)}`),

  resetPassword: (token: string, password: string) =>
    request<{ ok: true }>("/auth/password-reset", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ token, password }),
    }),

  logout: () => del("/auth/logout", "POST"),

  // ── Session PR linking (slice 7) ──────────────────────────────────────
  listSessionPrs: (id: string) =>
    request<{ prs: SessionPr[] }>(`/api/sessions/${enc(id)}/prs`).then(
      (r) => r.prs,
    ),

  linkSessionPr: (id: string, url: string) =>
    request<SessionPr>(`/api/sessions/${enc(id)}/prs`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ url }),
    }),

  unlinkSessionPr: (
    id: string,
    owner: string,
    repo: string,
    number: number,
  ) => {
    const sp = new URLSearchParams({ owner, repo, number: String(number) });
    return del(`/api/sessions/${enc(id)}/prs?${sp.toString()}`);
  },

  // ── Session Linear refs (auto-detected; no status/sync) ───────────────
  listSessionLinearDocs: (id: string) =>
    request<{ linear_docs: SessionLinearDoc[] }>(
      `/api/sessions/${enc(id)}/linear-docs`,
    ).then((r) => r.linear_docs),

  unlinkSessionLinearDoc: (id: string, url: string) =>
    del(`/api/sessions/${enc(id)}/linear-docs?${new URLSearchParams({ url })}`),

  // ── LLM model listing (slice 7) ───────────────────────────────────────
  llmModels: () =>
    request<{ models: LlmModel[] }>("/api/llm/models").then((r) => r.models),

  // ── PR activity (Wave 4) ──────────────────────────────────────────────
  /**
   * Cached PR conversation comments for a session's linked PRs, sorted by
   * `created_at` ascending (server-side). Unwraps the `comments` array.
   */
  listPrComments: (id: string, since?: number) =>
    request<{ comments: PrComment[] }>(
      `/api/sessions/${enc(id)}/prs/comments${since !== undefined ? `?since=${since}` : ""}`,
    ).then((r) => r.comments),

  listPrCommentPreviews: (id: string) =>
    request<{ comments: PrComment[] }>(
      `/api/sessions/${enc(id)}/prs/comments/preview`,
    ).then((r) => r.comments),

  /**
   * Post a reply on a linked PR. NOT relayed through the server: the browser
   * signs one `pr.comment` naming the exact repo, number, thread and body, and
   * the host runs its own `gh`. Rejects when the host refuses or is unreachable.
   */
  replyToPr: async (id: string, body: PrReplyRequest) => {
    await hostControl.commentOnPr(id, body);
    return { delivered: true } as PrReplyResult;
  },

  /**
   * Re-sync this session's linked PRs. The server's list only chooses which
   * pull requests to ASK about; each one is its own signed `pr.sync` naming the
   * exact target, so the host reads nothing the browser did not sign for.
   */
  syncPrs: async (id: string) => {
    const prs = await request<{
      prs: { owner: string; repo: string; number: number }[];
    }>(`/api/sessions/${enc(id)}/prs`).then((r) => r.prs);
    for (const pr of prs) {
      await hostControl.syncPr(id, pr);
    }
    return { delivered: true } as PrSyncResult;
  },

  /** Owner-only communication rail model. 403 for anyone but the owner. */
  listSessionCommunications: (id: string) =>
    request<SessionCommunications>(
      `/api/agent-messaging/sessions/${enc(id)}/communications`,
    ),
};
