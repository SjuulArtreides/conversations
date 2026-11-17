import * as React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, ApiError } from "@/lib/api";
import { streamChatMessage } from "@/lib/chatStream";
import { useAuth } from "@/lib/useAuth";
import { useLlmStatus } from "@/lib/useLlmStatus";
import { useDebounce } from "@/lib/useDebounce";
import { extractPrUrls } from "@/lib/prUrls";
import type { ChatMessage, SessionPr } from "@/lib/types";
import { humanizeTime } from "@/lib/utils";
import { Button } from "./ui/button";
import { Input } from "./ui/input";
import { Markdown } from "./Markdown";
import { Spinner } from "./states";
import { useToast } from "./ui/toast";

export type ChatParent =
  | { kind: "session"; id: string }
  | { kind: "project"; id: string };

interface ChatPanelProps {
  parent: ChatParent;
  /**
   * When true, renders as a collapsible right-docked panel rather than an
   * inline card. Open/closed state is persisted in localStorage under the
   * key `conversations.chatPanelOpen`. Defaults closed.
   */
  docked?: boolean;
  /**
   * Called whenever the docked panel's open/closed state changes. Allows the
   * parent layout to reserve space for the panel in the same render cycle.
   */
  onDockedToggle?: (open: boolean) => void;
}

/** localStorage key for the docked panel's open/closed state. */
const PANEL_OPEN_KEY = "conversations.chatPanelOpen";

/** localStorage key prefix for the chosen model per thread. */
const MODEL_KEY_PREFIX = "conversations.chatModel.";

/** Query key for the thread list of a given parent. */
function listKey(parent: ChatParent) {
  return ["chats", parent.kind, parent.id] as const;
}

/** Query key for session PRs. */
function sessionPrsKey(sessionId: string) {
  return ["session-prs", sessionId] as const;
}

/**
 * Chat threads attached to a session or a project: a thread list with a
 * create-thread affordance, and an open-thread view with a streaming composer,
 * share toggle (owner), and delete (owner). Renders nothing when server-side
 * LLM features are disabled.
 *
 * When `docked` is true the panel floats as a collapsible right rail. The
 * inline card is the default for project pages; docked is used on session
 * detail pages so the transcript/summary occupy the full main column.
 */
export function ChatPanel({ parent, docked = false, onDockedToggle }: ChatPanelProps) {
  const { enabled } = useLlmStatus();
  const [openChatId, setOpenChatId] = React.useState<string | null>(null);

  // --- docked panel open/closed state -----------------------------------------
  // Initialise from localStorage so the choice survives page reloads.
  const [panelOpen, setPanelOpen] = React.useState<boolean>(() => {
    if (!docked) return true;
    try {
      return localStorage.getItem(PANEL_OPEN_KEY) === "true";
    } catch {
      return false;
    }
  });

  // Sync every change back to localStorage and notify the parent layout.
  const togglePanel = React.useCallback((next: boolean) => {
    setPanelOpen(next);
    try {
      localStorage.setItem(PANEL_OPEN_KEY, String(next));
    } catch {
      // ignore — quota or private-browsing restriction
    }
    onDockedToggle?.(next);
  }, [onDockedToggle]);

  // --------------------------------------------------------------------------

  // Hide the whole panel when the server has no LLM configured.
  if (!enabled) return null;

  // --- DOCKED VARIANT -------------------------------------------------------
  if (docked) {
    return (
      <>
        {/* Launcher button — always visible in bottom-right. Shows thread
            count badge when collapsed so the user knows there's activity. */}
        {!panelOpen && (
          <button
            type="button"
            aria-label="Open chat panel"
            data-testid="chat-panel-launcher"
            onClick={() => togglePanel(true)}
            className="fixed bottom-6 right-6 z-50 flex h-11 items-center gap-2 rounded-full border border-border bg-card px-4 text-sm font-medium text-foreground shadow-lg transition-colors hover:bg-accent"
          >
            <svg
              className="h-4 w-4 shrink-0 text-primary"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
              aria-hidden
            >
              <path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" />
            </svg>
            Chat
          </button>
        )}

        {/* Docked right rail. On sm+ it's a 360px fixed panel; on phones it
            goes full-width. Its `bottom` tracks the on-screen keyboard
            (`--keyboard-inset`, published by useKeyboardInsetVar) so the
            composer pinned at the bottom stays visible above the keyboard
            instead of behind it. `top-0` + `bottom` define the height (no
            `h-full`), so the panel shrinks as the keyboard slides up. */}
        {panelOpen && (
          <div
            data-testid="chat-panel-rail"
            style={{ bottom: "var(--keyboard-inset, 0px)" }}
            className="fixed inset-x-0 top-0 z-40 flex w-full flex-col bg-card shadow-xl sm:inset-x-auto sm:right-0 sm:w-[360px] sm:border-l sm:border-border"
          >
            {/* Header */}
            <div className="flex shrink-0 items-center justify-between border-b border-border px-4 py-3">
              <h2 className="text-sm font-semibold text-foreground">Chat</h2>
              <button
                type="button"
                aria-label="Close chat panel"
                data-testid="chat-panel-close"
                onClick={() => togglePanel(false)}
                className="flex h-7 w-7 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
              >
                <svg
                  className="h-4 w-4"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="2.5"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  aria-hidden
                >
                  <path d="M18 6 6 18M6 6l12 12" />
                </svg>
              </button>
            </div>

            {/* Scrollable body */}
            <div className="min-h-0 flex-1 overflow-y-auto p-4">
              {openChatId == null ? (
                <ThreadList parent={parent} onOpen={setOpenChatId} />
              ) : (
                <ThreadView
                  chatId={openChatId}
                  parent={parent}
                  onBack={() => setOpenChatId(null)}
                  pinnedComposer
                />
              )}
            </div>
          </div>
        )}
      </>
    );
  }

  // --- INLINE VARIANT (project page, default) --------------------------------
  return (
    <div className="rounded-xl border border-border bg-card p-5">
      <h2 className="text-sm font-semibold text-foreground">Chat</h2>
      {openChatId == null ? (
        <ThreadList parent={parent} onOpen={setOpenChatId} />
      ) : (
        <ThreadView
          chatId={openChatId}
          parent={parent}
          onBack={() => setOpenChatId(null)}
        />
      )}
    </div>
  );
}

function ThreadList({
  parent,
  onOpen,
}: {
  parent: ChatParent;
  onOpen: (id: string) => void;
}) {
  const queryClient = useQueryClient();
  const { toast } = useToast();

  const { data, isLoading, isError, error } = useQuery({
    queryKey: listKey(parent),
    queryFn: () =>
      parent.kind === "session"
        ? api.sessionChats(parent.id)
        : api.projectChats(parent.id),
  });

  const create = useMutation({
    mutationFn: () =>
      parent.kind === "session"
        ? api.createSessionChat(parent.id)
        : api.createProjectChat(parent.id),
    onSuccess: (chat) => {
      queryClient.invalidateQueries({ queryKey: listKey(parent) });
      onOpen(chat.id);
    },
    onError: (err: Error) => toast(`Could not start chat: ${err.message}`, "error"),
  });

  return (
    <div className="mt-3 space-y-3">
      <div className="flex items-center justify-between gap-3">
        <p className="text-xs text-muted-foreground">
          Ask questions about this {parent.kind}.
        </p>
        <Button
          size="sm"
          variant="outline"
          onClick={() => create.mutate()}
          disabled={create.isPending}
        >
          {create.isPending ? "Starting…" : "New chat"}
        </Button>
      </div>

      {isLoading ? (
        <Spinner label="Loading chats…" />
      ) : isError ? (
        <p className="text-xs text-destructive">
          Could not load chats: {(error as Error)?.message}
        </p>
      ) : (data ?? []).length === 0 ? (
        <p className="text-xs text-muted-foreground">
          No chats yet. Start one to talk to the assistant.
        </p>
      ) : (
        <ul className="space-y-2">
          {data!.map((c) => (
            <li key={c.id}>
              <button
                type="button"
                onClick={() => onOpen(c.id)}
                className="flex w-full items-center justify-between gap-3 rounded-lg border border-border/70 px-3 py-2 text-left text-sm transition-colors hover:bg-accent"
              >
                <span className="truncate text-foreground">
                  {c.title || "Untitled chat"}
                  {c.shared && (
                    <span className="ml-2 text-[0.65rem] uppercase tracking-wide text-primary">
                      shared
                    </span>
                  )}
                </span>
                <span className="shrink-0 text-xs text-muted-foreground">
                  {humanizeTime(c.updated_at)}
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function ThreadView({
  chatId,
  parent,
  onBack,
  pinnedComposer = false,
}: {
  chatId: string;
  parent: ChatParent;
  onBack: () => void;
  /**
   * When true (the docked rail), pin the composer to the bottom of the
   * scrollable panel so it stays reachable above the mobile keyboard while the
   * messages scroll behind it. The inline variant leaves it in flow.
   */
  pinnedComposer?: boolean;
}) {
  const queryClient = useQueryClient();
  const { toast } = useToast();
  const { user } = useAuth();
  const { model: defaultModel } = useLlmStatus();

  // --- Model selection (persisted per thread) --------------------------------
  const modelStorageKey = `${MODEL_KEY_PREFIX}${chatId}`;
  const [selectedModel, setSelectedModel] = React.useState<string>(() => {
    try {
      return localStorage.getItem(modelStorageKey) ?? defaultModel ?? "";
    } catch {
      return defaultModel ?? "";
    }
  });

  // When the default model arrives from the server (async), seed the selection
  // if nothing is stored locally yet.
  React.useEffect(() => {
    if (defaultModel && !selectedModel) {
      setSelectedModel(defaultModel);
    }
  }, [defaultModel, selectedModel]);

  const handleModelChange = (model: string) => {
    setSelectedModel(model);
    try {
      localStorage.setItem(modelStorageKey, model);
    } catch {
      // ignore
    }
  };

  // --- Fetch linked PRs for the parent session (if any) ---------------------
  const linkedPrs = useQuery({
    queryKey: sessionPrsKey(parent.kind === "session" ? parent.id : ""),
    queryFn: () =>
      parent.kind === "session" ? api.listSessionPrs(parent.id) : Promise.resolve([]),
    enabled: parent.kind === "session",
  });

  const linkedUrls = React.useMemo(
    () => new Set((linkedPrs.data ?? []).map((p: SessionPr) => p.url)),
    [linkedPrs.data],
  );

  const linkPr = useMutation({
    mutationFn: (prUrl: string) => {
      if (parent.kind !== "session") throw new Error("Not a session");
      return api.linkSessionPr(parent.id, prUrl);
    },
    onSuccess: () => {
      if (parent.kind === "session") {
        queryClient.invalidateQueries({
          queryKey: sessionPrsKey(parent.id),
        });
      }
    },
    onError: (err: Error) => toast(`Could not link PR: ${err.message}`, "error"),
  });

  // --------------------------------------------------------------------------

  const { data, isLoading, isError, error } = useQuery({
    queryKey: ["chat", chatId],
    queryFn: () => api.getChat(chatId),
  });

  // Local message log: server history + any optimistic/streaming messages.
  const [draft, setDraft] = React.useState("");
  const [sending, setSending] = React.useState(false);
  const [localMessages, setLocalMessages] = React.useState<ChatMessage[]>([]);
  // The in-progress assistant text (null when no stream is active).
  const [streaming, setStreaming] = React.useState<string | null>(null);
  const bottomRef = React.useRef<HTMLDivElement | null>(null);

  // Seed the local log from the server history once it loads.
  React.useEffect(() => {
    if (data) setLocalMessages(data.messages);
  }, [data]);

  React.useEffect(() => {
    bottomRef.current?.scrollIntoView({ block: "nearest" });
  }, [localMessages, streaming]);

  const chat = data?.chat;
  const isOwner =
    !user || chat == null || chat.owner_email == null ||
    chat.owner_email === user.email;

  const shareToggle = useMutation({
    mutationFn: (shared: boolean) => api.updateChat(chatId, { shared }),
    onSuccess: (updated) => {
      queryClient.invalidateQueries({ queryKey: ["chat", chatId] });
      queryClient.invalidateQueries({ queryKey: listKey(parent) });
      toast(updated.shared ? "Chat shared" : "Chat unshared");
    },
    onError: (err: Error) => toast(`Could not update chat: ${err.message}`, "error"),
  });

  const remove = useMutation({
    mutationFn: () => api.deleteChat(chatId),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: listKey(parent) });
      onBack();
    },
    onError: (err: Error) => toast(`Could not delete chat: ${err.message}`, "error"),
  });

  async function send() {
    const content = draft.trim();
    if (!content || sending) return;
    setDraft("");
    setSending(true);
    // Optimistic user bubble.
    const optimisticUser: ChatMessage = {
      id: `local-${Date.now()}`,
      chat_id: chatId,
      role: "user",
      content,
      created_at: new Date().toISOString(),
    };
    setLocalMessages((prev) => [...prev, optimisticUser]);
    setStreaming("");

    let acc = "";
    try {
      await streamChatMessage(
        chatId,
        content,
        {
          onToken: (t) => {
            acc += t;
            setStreaming(acc);
          },
          onDone: (message) => {
            setStreaming(null);
            setLocalMessages((prev) => [...prev, message]);
            // Refresh persisted history (also picks up the persisted user msg).
            queryClient.invalidateQueries({ queryKey: ["chat", chatId] });
            queryClient.invalidateQueries({ queryKey: listKey(parent) });
          },
          onError: (message) => {
            setStreaming(null);
            toast(`Chat failed: ${message}`, "error");
          },
        },
        selectedModel || undefined,
      );
    } catch (err) {
      setStreaming(null);
      const message =
        err instanceof ApiError && err.status === 404
          ? "This chat is no longer available."
          : (err as Error)?.message ?? "Chat failed";
      toast(`Chat failed: ${message}`, "error");
    } finally {
      setSending(false);
    }
  }

  return (
    <div className="mt-3 space-y-4">
      <div className="flex items-center justify-between gap-3">
        <button
          type="button"
          onClick={onBack}
          className="text-xs text-muted-foreground transition-colors hover:text-foreground"
        >
          ← All chats
        </button>
        {chat && isOwner && (
          <div className="flex items-center gap-2">
            <Button
              size="sm"
              variant="outline"
              onClick={() => shareToggle.mutate(!chat.shared)}
              disabled={shareToggle.isPending}
            >
              {chat.shared ? "Unshare" : "Share"}
            </Button>
            <Button
              size="sm"
              variant="ghost"
              className="text-destructive hover:bg-destructive/10 hover:text-destructive"
              onClick={() => remove.mutate()}
              disabled={remove.isPending}
            >
              Delete
            </Button>
          </div>
        )}
      </div>

      {isLoading ? (
        <Spinner label="Loading chat…" />
      ) : isError ? (
        <p className="text-xs text-destructive">
          {error instanceof ApiError && error.status === 404
            ? "This chat is not available."
            : `Could not load chat: ${(error as Error)?.message}`}
        </p>
      ) : (
        <div className="space-y-3">
          {localMessages.length === 0 && streaming == null && (
            <p className="text-xs text-muted-foreground">
              No messages yet. Say hello.
            </p>
          )}
          {localMessages.map((m) => (
            <ChatBubble
              key={m.id}
              role={m.role}
              content={m.content}
              parent={parent}
              linkedUrls={linkedUrls}
              onLink={(url) => linkPr.mutate(url)}
            />
          ))}
          {streaming != null && (
            <ChatBubble
              role="assistant"
              content={streaming}
              pending
              parent={parent}
              linkedUrls={linkedUrls}
              onLink={(url) => linkPr.mutate(url)}
            />
          )}
          <div ref={bottomRef} />
        </div>
      )}

      <div
        className={
          pinnedComposer
            ? "sticky bottom-0 z-10 -mx-4 space-y-2 border-t border-border bg-card px-4 pb-1 pt-3"
            : "space-y-2"
        }
      >
        {/* Model picker — shown only when the LLM is enabled (we're inside ThreadView which is only rendered in that case). */}
        <ModelPicker
          selected={selectedModel}
          onSelect={handleModelChange}
        />

        <form
          className="flex items-end gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            void send();
          }}
        >
          <Input
            aria-label="Message"
            placeholder="Send a message…"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            disabled={sending}
          />
          <Button type="submit" disabled={sending || !draft.trim()}>
            {sending ? "Sending…" : "Send"}
          </Button>
        </form>
      </div>
    </div>
  );
}

/**
 * A searchable combobox that fetches from `GET /api/llm/models` and lets the
 * user filter + select a model slug. Gracefully hidden when the endpoint 503s
 * (LLM disabled at the infra level — the panel itself should already be hidden
 * in that case, but this provides a belt-and-suspenders guard).
 */
function ModelPicker({
  selected,
  onSelect,
}: {
  selected: string;
  onSelect: (model: string) => void;
}) {
  const [query, setQuery] = React.useState("");
  const [open, setOpen] = React.useState(false);
  const debounced = useDebounce(query, 150);
  const containerRef = React.useRef<HTMLDivElement | null>(null);

  const { data: models, isError } = useQuery({
    queryKey: ["llm-models"],
    queryFn: api.llmModels,
    staleTime: 300_000,
    retry: false,
  });

  // Close on outside click.
  React.useEffect(() => {
    function onPointerDown(e: PointerEvent) {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    }
    document.addEventListener("pointerdown", onPointerDown);
    return () => document.removeEventListener("pointerdown", onPointerDown);
  }, []);

  // If the endpoint 503d or returned nothing, don't render the picker.
  if (isError || !models || models.length === 0) return null;

  const lowerQuery = debounced.toLowerCase();
  const filtered = lowerQuery
    ? models.filter(
        (m) =>
          m.id.toLowerCase().includes(lowerQuery) ||
          m.name.toLowerCase().includes(lowerQuery),
      )
    : models;

  const selectedModel = models.find((m) => m.id === selected);
  const displayLabel = selectedModel?.name ?? selected ?? "Select model";

  return (
    <div ref={containerRef} className="relative">
      <button
        type="button"
        aria-label="Select model"
        data-testid="model-picker-trigger"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center justify-between gap-2 rounded-md border border-border bg-background px-2.5 py-1 text-left text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
      >
        <span className="truncate">{displayLabel}</span>
        <svg
          className="h-3 w-3 shrink-0 opacity-60"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2.5"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden
        >
          <path d="m6 9 6 6 6-6" />
        </svg>
      </button>

      {open && (
        <div className="absolute bottom-full left-0 z-50 mb-1 w-full min-w-[240px] rounded-lg border border-border bg-card shadow-xl">
          <div className="p-2">
            <Input
              type="text"
              aria-label="Filter models"
              placeholder="Filter models…"
              value={query}
              autoFocus
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape") setOpen(false);
                if (e.key === "Enter" && filtered.length > 0) {
                  onSelect(filtered[0].id);
                  setOpen(false);
                  setQuery("");
                }
              }}
              className="h-7 text-xs"
            />
          </div>
          <ul
            className="max-h-48 overflow-y-auto pb-1"
            role="listbox"
            aria-label="Available models"
          >
            {filtered.length === 0 ? (
              <li className="px-3 py-2 text-xs text-muted-foreground">
                No models match "{debounced}"
              </li>
            ) : (
              filtered.map((m) => (
                <li key={m.id}>
                  <button
                    type="button"
                    role="option"
                    aria-selected={m.id === selected}
                    onClick={() => {
                      onSelect(m.id);
                      setOpen(false);
                      setQuery("");
                    }}
                    className={`flex w-full items-center justify-between gap-3 px-3 py-1.5 text-left text-xs transition-colors hover:bg-accent${m.id === selected ? " bg-accent/50 font-medium text-foreground" : " text-muted-foreground"}`}
                  >
                    <span className="truncate">{m.name}</span>
                    {m.context_length > 0 && (
                      <span className="shrink-0 opacity-60">
                        {(m.context_length / 1000).toFixed(0)}k
                      </span>
                    )}
                  </button>
                </li>
              ))
            )}
          </ul>
        </div>
      )}
    </div>
  );
}

function ChatBubble({
  role,
  content,
  pending,
  parent,
  linkedUrls,
  onLink,
}: {
  role: "user" | "assistant";
  content: string;
  pending?: boolean;
  parent: ChatParent;
  linkedUrls: Set<string>;
  onLink: (url: string) => void;
}) {
  const isUser = role === "user";
  const isSession = parent.kind === "session";

  // Detect PR URLs in the message content.
  const prs = React.useMemo(() => extractPrUrls(content), [content]);

  return (
    <div className={isUser ? "flex justify-end" : "flex justify-start"}>
      <div
        className={
          isUser
            ? "max-w-[85%] rounded-lg bg-primary/15 px-3 py-2 text-sm text-foreground"
            : "max-w-[85%] rounded-lg border border-border/70 px-3 py-2 text-sm text-foreground"
        }
      >
        {isUser ? (
          <p className="whitespace-pre-wrap">{content}</p>
        ) : content ? (
          <Markdown>{content}</Markdown>
        ) : (
          <span className="text-muted-foreground">…</span>
        )}
        {pending && content && (
          <span className="ml-0.5 inline-block animate-pulse">▍</span>
        )}

        {/* PR chips — shown when there are detected PRs in this message */}
        {prs.length > 0 && (
          <div className="mt-2 flex flex-wrap gap-1.5">
            {prs.map((pr) => {
              const isLinked = linkedUrls.has(pr.url);
              return (
                <PrChip
                  key={pr.url}
                  owner={pr.owner}
                  repo={pr.repo}
                  number={pr.number}
                  url={pr.url}
                  isLinked={isLinked}
                  showLink={isSession && !isLinked}
                  onLink={() => onLink(pr.url)}
                />
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}

/** A chip badge for a detected PR URL with an optional "Link" button. */
function PrChip({
  owner,
  repo,
  number,
  url,
  isLinked,
  showLink,
  onLink,
}: {
  owner: string;
  repo: string;
  number: number;
  url: string;
  isLinked: boolean;
  showLink: boolean;
  onLink: () => void;
}) {
  return (
    <span
      className={`inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-[0.65rem] font-medium${isLinked ? " border-primary/40 bg-primary/10 text-primary" : " border-border bg-muted/40 text-muted-foreground"}`}
      data-testid="pr-chip"
    >
      <a
        href={url}
        target="_blank"
        rel="noopener noreferrer"
        className="hover:underline"
        onClick={(e) => e.stopPropagation()}
        aria-label={`${owner}/${repo} PR #${number}`}
      >
        {owner}/{repo}#{number}
      </a>
      {showLink && (
        <button
          type="button"
          data-testid="pr-link-button"
          onClick={(e) => {
            e.stopPropagation();
            onLink();
          }}
          className="rounded px-1 py-px text-[0.6rem] font-semibold text-primary transition-colors hover:bg-primary/10"
        >
          Link
        </button>
      )}
      {isLinked && (
        <span className="opacity-70" aria-label="linked">
          ✓
        </span>
      )}
    </span>
  );
}

// Re-export session PRs query key so SessionDetail can invalidate it.
export { sessionPrsKey };

// Export type for use in SessionDetail
export type { SessionPr };
