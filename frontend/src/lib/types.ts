export type AgentReadiness =
  "unknown" | "working" | "background" | "needs_input" | "ready";

export type Visibility = "organization" | "restricted";

export type AgentKind = "claude" | "codex" | "opencode" | "zsh";

export const ALL_AGENTS: AgentKind[] = ["claude", "codex", "opencode", "zsh"];

export interface AppInfo {
  version: string;
}

export type CodexSandboxMode =
  "read-only" | "workspace-write" | "danger-full-access";

export type CodexApprovalPolicy =
  "untrusted" | "on-failure" | "on-request" | "never";

export const CODEX_SANDBOX_MODES: CodexSandboxMode[] = [
  "read-only",
  "workspace-write",
  "danger-full-access",
];

export const CODEX_APPROVAL_POLICIES: CodexApprovalPolicy[] = [
  "untrusted",
  "on-failure",
  "on-request",
  "never",
];

export const agentLabel = (agent: AgentKind): string => {
  if (agent === "codex") return "Codex";
  if (agent === "opencode") return "OpenCode";
  if (agent === "zsh") return "Zsh";
  return "Claude";
};

/**
 * How an agent is started. `"opencode"` (the default) means the agent's own
 * binary; `"adk"` runs OpenCode through the Arrakis dev harness (`adk code`),
 * which adds the monorepo's MCP credentials and agent profile. The session it
 * produces is an OpenCode session either way — the `agent` stays `"opencode"`
 * and only the spawn differs — so this is NOT another [`AgentKind`].
 */
export type AgentLauncher = "opencode" | "adk";

/** One entry in the "start a session" menu: an agent plus how to start it. */
export interface LaunchTarget {
  agent: AgentKind;
  /** Omitted ⇒ the agent's own binary. */
  launcher?: AgentLauncher;
}

/**
 * Every way a session can be started, in menu order. `adk code` sits next to
 * plain OpenCode because it is the same agent behind a different launcher.
 */
export const ALL_LAUNCH_TARGETS: LaunchTarget[] = [
  { agent: "claude" },
  { agent: "codex" },
  { agent: "opencode" },
  { agent: "opencode", launcher: "adk" },
  { agent: "zsh" },
];

/** Stable key for a target (React lists, test lookups). */
export const launchTargetKey = (target: LaunchTarget): string =>
  target.launcher && target.launcher !== "opencode"
    ? `${target.agent}:${target.launcher}`
    : target.agent;

/**
 * Menu label. `adk code` is named after the command, not the agent: the point
 * of picking it is the harness, and the resulting session already shows an
 * "OpenCode" badge.
 */
export const launchTargetLabel = (target: LaunchTarget): string =>
  target.launcher === "adk" ? "adk code" : agentLabel(target.agent);

export const sameLaunchTarget = (a: LaunchTarget, b: LaunchTarget): boolean =>
  launchTargetKey(a) === launchTargetKey(b);

/** One user's local checkout path for a repo+branch. */
export interface RepoPath {
  user_email: string;
  local_path: string;
}

/** A repo+branch a conversation touched, with every user's local path. */
export interface RepoLink {
  repo: string;
  remote_url: string | null;
  branch: string;
  pr_url: string | null;
  paths: RepoPath[];
  /**
   * True when this link is one of the requesting viewer's configured monorepo
   * "container" repos (computed server-side per viewer; excluded from PR
   * detection and hidden/de-emphasized in the UI). Absent on older server
   * responses — treat `undefined` as `false`.
   */
  is_container?: boolean;
}

/**
 * Inline-code colour config for Slack-friendly terminal copy — the foreground
 * colours that mark a cell as inline code. `palette` are ANSI/256 indices;
 * `rgb` are packed `0xRRGGBB` truecolor values. Mirrors the server blob and the
 * `localStorage.arrakisTermCodeColors` override shape.
 */
export interface InlineCodeColors {
  palette?: number[];
  rgb?: number[];
}

/** A user's per-account settings (`GET`/`PUT /api/user/settings`). */
export interface UserSettings {
  /** Absolute "container" repo paths treated as monorepo containers, not work repos. */
  container_paths: string[];
  /** Agent used when a new session is started without an explicit override. */
  default_agent?: AgentKind;
  default_launcher?: AgentLauncher;
  /** Inline-code colour config for terminal copy; absent ⇒ client default. */
  inline_code_colors?: InlineCodeColors | null;
}

/** Canonical repo-link fields editable from the web UI (PUT body shape). */
export interface RepoLinkInput {
  repo: string;
  remote_url: string | null;
  branch: string;
  pr_url: string | null;
}

export interface Summary {
  id: string;
  agent?: AgentKind;
  title: string;
  project_path: string | null;
  git_branch: string | null;
  message_count: number;
  started_at: string | null;
  ended_at: string | null;
  uploaded_by: string;
  uploaded_at: string;
  source_host: string | null;
  owner_email: string | null;
  visibility: Visibility;
  /**
   * True when a live session is currently streaming for this id. The list
   * renders these as LIVE cards. Absent on older server responses — treat
   * `undefined` as `false`.
   */
  is_live?: boolean;
  repos: RepoLink[];
  /**
   * True when this session's launch directory (`project_path`) is one of the
   * viewer's configured monorepo "container" paths — i.e. it was started at a
   * container root, not inside a work sub-repo. Computed server-side per viewer.
   * The card/detail use it to suppress the transcript-branch fallback, whose
   * branch is the container's (not a work repo's). Absent ⇒ treat as `false`.
   */
  project_is_container?: boolean;
}

export interface Usage {
  input_tokens?: number;
  output_tokens?: number;
  cache_read_input_tokens?: number;
  cache_creation_input_tokens?: number;
}

export type MessageKind =
  "text" | "thinking" | "tool_use" | "tool_result" | "mixed";

export type Role = "user" | "assistant";

export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "thinking"; thinking: string }
  | { type: "tool_use"; id: string; name: string; input: unknown }
  | { type: "tool_result"; tool_use_id: string; content: string }
  | { type: "other"; [key: string]: unknown };

export interface Message {
  uuid: string;
  parent_uuid: string | null;
  role: Role;
  kind: MessageKind;
  model: string | null;
  is_sidechain: boolean;
  timestamp: string | null;
  blocks: ContentBlock[];
  text: string;
  usage: Usage | null;
}

export interface SessionSummary {
  intent: string;
  invariants: string[];
  summary: string;
  model: string;
  generated_by: string | null;
  generated_at: string;
}

export interface LlmStatus {
  enabled: boolean;
  model: string | null;
}

export interface Session extends Summary {
  cwd: string | null;
  claude_version: string | null;
  skipped_lines: number;
  shared_emails: string[];
  messages: Message[];
  summary: SessionSummary | null;
  /** The explicit project this session is assigned to, if any (slice 5a). */
  project_id: string | null;
  /** Startup state when this session was created by an agent handoff. */
  handoff?: SessionHandoff | null;
  /** Set when a host publishes this session; only its host may change it. */
  publication?: SessionPublication | null;
  /**
   * True when the VIEWER was named by the publishing host as someone intended
   * to type into the session through it, once their key is trusted there.
   * Until then they read the published copy and cannot write
   * until key trust is established. Absent on older servers ⇒ false.
   */
  viewer_write_pending_key?: boolean;
}

export interface SessionPublication {
  state: "active" | "deleted";
  rev: number;
  /** The publishing host: for its owner, or anyone when auth is off. Grants nothing. */
  host_id?: string;
}

export interface SessionHandoff {
  source_session_id: string;
  status: "requested" | "starting" | "started" | "failed";
  error: string | null;
  created_at: string;
}

export interface ProjectInfo {
  project_path: string;
  session_count: number;
}

export interface UserInfo {
  email: string;
  display_name: string | null;
  created_at: string;
  last_seen_at: string;
}

export interface SharingPayload {
  reason?: string;
  visibility: Visibility;
  emails: string[];
}

/** The logged-in user, as returned by `/api/auth/config` and the auth endpoints. */
export interface CurrentUser {
  email: string;
  display_name: string | null;
  is_admin?: boolean;
}

export interface PasswordResetLink {
  reset_path: string;
  expires_at: string;
}

export interface AdminUser {
  email: string;
  is_initial: boolean;
}

export interface AdminDirectoryUser {
  email: string;
  display_name: string | null;
  is_admin: boolean;
  is_initial: boolean;
}

/**
 * Auth state for the SPA. `null` (from a 404 on `/api/auth/config`) means auth
 * is disabled server-side — the SPA then shows no login UI at all.
 */
export interface AuthConfig {
  google_enabled: boolean;
  password_enabled: boolean;
  user: CurrentUser | null;
}

// ── Live sessions (streaming-now) ──────────────────────────────────────────

/** Whether a live session is being watched read-only or can be replied into. */
export type LiveMode = "view" | "interactive";

/**
 * A session that is currently streaming live (from `GET /api/live`). `id` and
 * `session_id` are equal; once the session graduates it becomes an ordinary
 * stored session at `/sessions/{session_id}`.
 */
export interface LiveSession {
  id: string;
  session_id: string;
  agent?: AgentKind;
  title: string;
  project_path: string | null;
  started_at: string | null;
  last_event_at: string | null;
  /** `"interactive"` sessions accept replies injected as keystrokes. */
  mode: LiveMode;
  /** True when the authenticated viewer authored this live session. */
  viewer_is_author: boolean;
  /**
   * True when the viewer may interact (author, or a read+write sharee). Write is
   * the single interactive grant: it covers sanitized replies, raw
   * keyboard/clipboard input (`/input`, `/clipboard`), and terminal control
   * requests (`/repaint`, `/resize`) — there is no separate keyboard-control
   * grant. Computed per-request from authorship + share level; never stored.
   */
  viewer_can_write: boolean;
  /**
   * True when the session's local host is currently attached to the
   * session's interactive streams. For interactive sessions this means raw
   * keyboard input is deliverable, not merely that the reply stream exists.
   * May be absent on older server responses; treat `undefined` as attached.
   */
  cli_attached?: boolean;
  /**
   * ISO 8601 timestamp of the last visible terminal text change. Repeated
   * redraws and cursor-only updates do not refresh it. `null`/absent means no
   * screen text has changed since the server started. Unlike this timestamp,
   * `last_event_at` is refreshed by heartbeats even while a session is idle.
   */
  last_pty_at?: string | null;
  /**
   * PRs auto-linked (or manually linked) to this live session, with the same
   * cached open/merged/closed `status` as the per-session PR list. Lets search
   * match a pasted PR URL / `owner/repo#N` against live sessions. May be absent
   * on older server responses — treat `undefined` as `[]`.
   */
  linked_prs?: SessionPr[];
}

/**
 * One entry in a session's person share list. Transcript access and
 * communication are independent; at least one must be granted.
 */
export interface LiveShare {
  email: string;
  /**
   * Read the transcript and watch the live stream. Absent on older server
   * responses: read unless `communication_only` is true.
   */
  can_read?: boolean;
  /**
   * Interactive access: sanitized replies, raw keyboard/clipboard input, and
   * terminal control requests. Write is the single interactive grant — there is
   * no separate keyboard control. Requires `can_read`.
   */
  can_write: boolean;
  /**
   * The person's own sessions may discover and message this session. Grants no
   * transcript or terminal access. Absent on older server responses, where
   * read-write and communication-only shares communicated.
   */
  can_communicate?: boolean;
  /** Legacy server field: `true` means no transcript access. */
  communication_only?: boolean;
}

/**
 * Result of a live control POST that injects into the PTY — `/clipboard` (paste
 * image). Keystrokes, resize and repaint are signed host operations, not server routes. `delivered:false` means no live CLI is attached to inject into.
 */
export interface PostInputResult {
  delivered: boolean;
}

/** A file relayed to the CLI host (`POST /api/live/{id}/files`). */
export interface LiveFilePayload {
  name: string;
  b64: string;
}

/** `status: "sent"` means the host acknowledged delivery of every file. */
export interface SendLiveFilesResult {
  status: "sent";
}

/**
 * One decoded frame from `GET /api/live/{id}/stream`.
 *
 * The server replays every stored message (as `message` frames, `seq`-ordered)
 * before streaming live deltas, then emits a single terminal `complete` frame
 * when the session genuinely ends. The `seq` lives on the ENVELOPE, not on the
 * `Message` — read it from here. A `message` frame's `message` is the same
 * shape as `Session.messages[number]`.
 */
export type LiveFrame =
  | { type: "message"; seq: number; message: Message }
  // `?bodies=false` replaces each `message` frame with this outline. `visible`
  // is false for a message the transcript folds away (`filterRenderedMessages`).
  | { type: "message_outline"; seq: number; role: Role; visible: boolean }
  | { type: "complete"; session_id: string }
  | { type: "pty"; cols: number; rows: number; b64: string }
  // The server auto-detected & linked one or more new PR refs for this session.
  // A refetch nudge (no PR payload) — consumers reload the session's PR list.
  | { type: "prs_linked"; session_id: string }
  | { type: "pr_comments_changed" }
  // Same, for Linear refs (document / issue / project) — reload the linear list.
  | { type: "linear_docs_linked"; session_id: string };

// ── Explicit, user-named projects + chat (slice 5/6) ───────────────────────

/** An explicit, user-named project that groups sessions and carries chats. */
export interface Project {
  id: string;
  name: string;
  description: string | null;
  owner_email: string | null;
  session_count: number;
  created_at: string;
  updated_at: string;
}

/** A single project plus its member sessions (reuses the list summary shape). */
export interface ProjectDetail {
  project: Project;
  sessions: Pick<
    Summary,
    "id" | "title" | "git_branch" | "project_path" | "visibility"
  >[];
}

/** A chat thread, attached to either a session or a project (never both). */
export interface Chat {
  id: string;
  session_id: string | null;
  project_id: string | null;
  owner_email: string | null;
  title: string | null;
  shared: boolean;
  created_at: string;
  updated_at: string;
}

export type ChatRole = "user" | "assistant";

/** One persisted message in a chat thread. */
export interface ChatMessage {
  id: string;
  chat_id: string;
  role: ChatRole;
  content: string;
  created_at: string;
}

/** A chat plus its full message history (the `GET /api/chats/{id}` shape). */
export interface ChatWithMessages {
  chat: Chat;
  messages: ChatMessage[];
}

// ── PR linking (slice 7) ───────────────────────────────────────────────────

/** Status information for a linked pull request from GitHub. */
export interface PrStatus {
  review_decision?: string | null;
  /** gh's raw `state`, passed through verbatim by the CLI/server. */
  state: "OPEN" | "CLOSED" | "MERGED";
  merged: boolean;
  draft: boolean;
  title: string;
}

/** A GitHub PR linked to a session. */
export interface SessionPr {
  url: string;
  owner: string;
  repo: string;
  number: number;
  status: PrStatus | null;
  /**
   * RFC3339 timestamp of when the per-user CLI (`gh`) last synced this PR's
   * status snapshot, or `null` if it has never been synced. May be absent on
   * older server responses — treat `undefined` like `null`.
   */
  status_fetched_at?: string | null;
  /**
   * Whether this PR was auto-detected from the conversation text (`true`) or
   * linked manually (`false`). May be absent on older server responses —
   * treat `undefined` like `false`.
   */
  detected?: boolean;
}

/**
 * A Linear reference (document / issue / project) linked to a session — the
 * Linear analogue of {@link SessionPr}, but a strict subset: the server holds no
 * Linear credentials, so there is no status/sync. `url` is the canonical
 * `https://linear.app/{workspace}/{kind}/{identifier}` link; `kind` is
 * `"document" | "issue" | "project"`; `identifier` is the human-readable handle
 * (`TEAM-123` for an issue, the `{slug-id}` segment for a document/project).
 */
export interface SessionLinearDoc {
  url: string;
  workspace: string;
  kind: string;
  identifier: string;
}

// ── LLM model picker (slice 7) ─────────────────────────────────────────────

/** One LLM model returned by `GET /api/llm/models`. */
export interface LlmModel {
  id: string;
  name: string;
  context_length: number;
}

// ── PR activity (Wave 4) ───────────────────────────────────────────────────

/** The kind of GitHub comment cached for a linked PR. */
export type PrCommentKind = "issue" | "review" | "review_comment";

/**
 * One cached PR conversation comment, as returned by
 * `GET /api/sessions/{id}/prs/comments`. The server caches comments pushed by
 * the per-user CLI (`gh`) and returns them sorted by `created_at` ascending.
 */
export interface PrComment {
  owner: string;
  repo: string;
  number: number;
  comment_id: string;
  kind: string;
  author: string;
  body: string;
  html_url: string;
  created_at: string;
  ingested_at: string;
  /** Monotonic per-session revision for incremental comment reads. */
  revision?: number;
  /**
   * Inline review-comment context — populated only for `kind ===
   * "review_comment"` (fetched from GitHub's GraphQL review threads). Absent
   * (`null`/`undefined`) for issue / review kinds and for rows pushed by an
   * older CLI. `line` falls back to the comment's original line when the thread
   * is outdated; `resolved` is the thread's resolution state.
   */
  path?: string | null;
  line?: number | null;
  diff_hunk?: string | null;
  resolved?: boolean | null;
}

/**
 * Body for `POST /api/sessions/{id}/prs/reply` — relay a reply to the linked
 * PR via the attached CLI (which runs `gh pr comment`).
 */
export interface PrReplyRequest {
  owner: string;
  repo: string;
  number: number;
  body: string;
  /**
   * When set, the GitHub `databaseId` of a review comment to reply to in its
   * thread — the CLI posts via `gh api …/comments/{id}/replies` instead of a
   * top-level `gh pr comment`. Absent ⇒ a top-level PR comment.
   */
  in_reply_to?: number;
}

/**
 * Result of relaying a PR reply (`POST /api/sessions/{id}/prs/reply`).
 * `delivered:false` means no live CLI is attached to run `gh pr comment`.
 */
export interface PrReplyResult {
  delivered: boolean;
}

/**
 * Result of relaying a PR sync (`POST /api/sessions/{id}/prs/sync`).
 * `delivered:false` means no CLI agent (`host`) is attached
 * to run `gh` and push the refreshed status + comments.
 */
export interface PrSyncResult {
  delivered: boolean;
}

export interface LiveOrder {
  revision: number;
  session_ids: string[];
}

export interface SessionNotification {
  id: string;
  session_id: string;
  kind: "share" | "attention";
  sender_email: string;
  recipient_email: string;
  message: string;
  created_at: string;
  read_at: string | null;
  resolved_at: string | null;
  session_title: string | null;
}

export interface AttentionRecipient {
  email: string;
  display_name: string | null;
}

/** Message metadata between the current session and one peer, under current
 *  authorization, including first-line message titles. */
export interface CommunicationConversation {
  sent_count: number;
  received_count: number;
  last_message_at: string;
  last_direction: "sent" | "received";
  /** Status of the newest message this session sent; null when it sent none.
   *  `queued` = stored, `received` = fetched by the recipient's client,
   *  `acknowledged` = accepted or replied to. None proves work completed. */
  last_sent_status: "queued" | "received" | "acknowledged" | null;
  /** Incoming messages this session has not acknowledged. */
  unacknowledged_incoming: number;
  messages: {
    id: string;
    title: string;
    direction: "sent" | "received";
    created_at: string;
  }[];
}

/** One session the current session may message (the owner-only rail model). */
export interface CommunicationPeer {
  session_id: string;
  title: string;
  agent: AgentKind;
  owner_email: string | null;
  last_seen_at: string;
  /** A CLI stream is attached; not proof the agent is processing messages. */
  connected: boolean;
  /** `live`: a CLI stream is attached. `idle`: registered live, nothing
   *  attached, so a message waits until its client fetches it. `ended`: no
   *  live registration. */
  state: "live" | "idle" | "ended";
  /** The server's verdict on whether a message sent now can be received: not
   *  ended, or an ended session of the viewer's own (it reopens to the same id). */
  reachable: boolean;
  /** The viewer can read this session's transcript. */
  readable: boolean;
  conversation: CommunicationConversation | null;
}

/** `GET /api/agent-messaging/sessions/{id}/communications`. */
export interface SessionCommunications {
  project_id: string | null;
  owner_email: string | null;
  sessions: CommunicationPeer[];
}
