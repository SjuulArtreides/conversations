import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { ChatPanel } from "./ChatPanel";

// Mock the API module the panel depends on.
vi.mock("@/lib/api", async () => {
  const actual = await vi.importActual<typeof import("@/lib/api")>("@/lib/api");
  return {
    ...actual,
    api: {
      sessionChats: vi.fn().mockResolvedValue([
        {
          id: "chat-1",
          session_id: "s1",
          project_id: null,
          owner_email: null,
          title: "Deploy questions",
          shared: false,
          created_at: "2026-06-06T00:00:00Z",
          updated_at: "2026-06-06T00:00:00Z",
        },
      ]),
      createSessionChat: vi.fn(),
      getChat: vi.fn(),
      listSessionPrs: vi.fn().mockResolvedValue([]),
      linkSessionPr: vi.fn(),
      llmModels: vi.fn().mockResolvedValue([
        { id: "claude-opus-4-5", name: "Claude Opus 4.5", context_length: 200000 },
        { id: "claude-sonnet-4-5", name: "Claude Sonnet 4.5", context_length: 200000 },
      ]),
    },
  };
});

// LLM status drives whether the panel renders at all.
vi.mock("@/lib/useLlmStatus", () => ({
  useLlmStatus: vi.fn(),
}));

import { api } from "@/lib/api";
import { useLlmStatus } from "@/lib/useLlmStatus";

const mockedLlm = vi.mocked(useLlmStatus);

beforeEach(() => {
  vi.clearAllMocks();
  // Re-apply default mocks that clearAllMocks wipes.
  vi.mocked(api.sessionChats).mockResolvedValue([
    {
      id: "chat-1",
      session_id: "s1",
      project_id: null,
      owner_email: null,
      title: "Deploy questions",
      shared: false,
      created_at: "2026-06-06T00:00:00Z",
      updated_at: "2026-06-06T00:00:00Z",
    },
  ]);
  vi.mocked(api.listSessionPrs).mockResolvedValue([]);
  vi.mocked(api.llmModels).mockResolvedValue([
    { id: "claude-opus-4-5", name: "Claude Opus 4.5", context_length: 200000 },
    { id: "claude-sonnet-4-5", name: "Claude Sonnet 4.5", context_length: 200000 },
  ]);
  // Clear any persisted chat-panel state between tests.
  localStorage.clear();
});

describe("ChatPanel", () => {
  it("renders nothing when LLM features are disabled", () => {
    mockedLlm.mockReturnValue({ enabled: false, model: null, isLoading: false });
    renderWithProviders(<ChatPanel parent={{ kind: "session", id: "s1" }} />);
    // Panel returns null: no "Chat" heading and no thread fetch.
    expect(screen.queryByText("Chat")).not.toBeInTheDocument();
    expect(api.sessionChats).not.toHaveBeenCalled();
  });

  it("lists the session's chat threads when LLM is enabled", async () => {
    mockedLlm.mockReturnValue({
      enabled: true,
      model: "claude",
      isLoading: false,
    });
    renderWithProviders(<ChatPanel parent={{ kind: "session", id: "s1" }} />);

    expect(await screen.findByText("Deploy questions")).toBeInTheDocument();
    await waitFor(() =>
      expect(api.sessionChats).toHaveBeenCalledWith("s1"),
    );
  });

  it("opens a thread and shows its messages", async () => {
    mockedLlm.mockReturnValue({
      enabled: true,
      model: "claude",
      isLoading: false,
    });
    vi.mocked(api.getChat).mockResolvedValue({
      chat: {
        id: "chat-1",
        session_id: "s1",
        project_id: null,
        owner_email: null,
        title: "Deploy questions",
        shared: false,
        created_at: "2026-06-06T00:00:00Z",
        updated_at: "2026-06-06T00:00:00Z",
      },
      messages: [
        {
          id: "m1",
          chat_id: "chat-1",
          role: "user",
          content: "How do I deploy?",
          created_at: "2026-06-06T00:00:00Z",
        },
      ],
    });

    const user = userEvent.setup();
    renderWithProviders(<ChatPanel parent={{ kind: "session", id: "s1" }} />);

    await user.click(await screen.findByText("Deploy questions"));
    expect(await screen.findByText("How do I deploy?")).toBeInTheDocument();
    expect(screen.getByLabelText("Message")).toBeInTheDocument();
  });
});

describe("ChatPanel — docked variant", () => {
  it("is collapsed by default: launcher visible, rail hidden", () => {
    mockedLlm.mockReturnValue({ enabled: true, model: "claude", isLoading: false });
    renderWithProviders(
      <ChatPanel parent={{ kind: "session", id: "s1" }} docked />,
    );
    // Launcher button must be present.
    expect(screen.getByTestId("chat-panel-launcher")).toBeInTheDocument();
    // The rail (open panel) must not be rendered.
    expect(screen.queryByTestId("chat-panel-rail")).not.toBeInTheDocument();
  });

  it("clicking the launcher opens the panel and hides the launcher", async () => {
    mockedLlm.mockReturnValue({ enabled: true, model: "claude", isLoading: false });
    const user = userEvent.setup();
    renderWithProviders(
      <ChatPanel parent={{ kind: "session", id: "s1" }} docked />,
    );

    await user.click(screen.getByTestId("chat-panel-launcher"));

    // Rail is now present.
    expect(screen.getByTestId("chat-panel-rail")).toBeInTheDocument();
    // Launcher is gone.
    expect(screen.queryByTestId("chat-panel-launcher")).not.toBeInTheDocument();
    // Chat heading is visible inside the rail.
    expect(screen.getByText("Chat")).toBeInTheDocument();
  });

  it("clicking the close button collapses the panel", async () => {
    mockedLlm.mockReturnValue({ enabled: true, model: "claude", isLoading: false });
    const user = userEvent.setup();
    renderWithProviders(
      <ChatPanel parent={{ kind: "session", id: "s1" }} docked />,
    );

    // Open it first.
    await user.click(screen.getByTestId("chat-panel-launcher"));
    expect(screen.getByTestId("chat-panel-rail")).toBeInTheDocument();

    // Then close it.
    await user.click(screen.getByTestId("chat-panel-close"));
    expect(screen.queryByTestId("chat-panel-rail")).not.toBeInTheDocument();
    expect(screen.getByTestId("chat-panel-launcher")).toBeInTheDocument();
  });

  it("persists open state to localStorage when opened", async () => {
    mockedLlm.mockReturnValue({ enabled: true, model: "claude", isLoading: false });
    const user = userEvent.setup();
    renderWithProviders(
      <ChatPanel parent={{ kind: "session", id: "s1" }} docked />,
    );

    await user.click(screen.getByTestId("chat-panel-launcher"));

    expect(localStorage.getItem("conversations.chatPanelOpen")).toBe("true");
  });

  it("persists closed state to localStorage when closed", async () => {
    mockedLlm.mockReturnValue({ enabled: true, model: "claude", isLoading: false });
    // Pre-open the panel via localStorage so we start open.
    localStorage.setItem("conversations.chatPanelOpen", "true");
    const user = userEvent.setup();
    renderWithProviders(
      <ChatPanel parent={{ kind: "session", id: "s1" }} docked />,
    );

    // Should start open.
    expect(screen.getByTestId("chat-panel-rail")).toBeInTheDocument();

    await user.click(screen.getByTestId("chat-panel-close"));

    expect(localStorage.getItem("conversations.chatPanelOpen")).toBe("false");
  });

  it("restores open state from localStorage on mount", () => {
    mockedLlm.mockReturnValue({ enabled: true, model: "claude", isLoading: false });
    // Seed localStorage as if the user previously opened the panel.
    localStorage.setItem("conversations.chatPanelOpen", "true");
    renderWithProviders(
      <ChatPanel parent={{ kind: "session", id: "s1" }} docked />,
    );

    // Panel should start open: rail visible, launcher hidden.
    expect(screen.getByTestId("chat-panel-rail")).toBeInTheDocument();
    expect(screen.queryByTestId("chat-panel-launcher")).not.toBeInTheDocument();
  });

  it("calls onDockedToggle with the new state when toggled", async () => {
    mockedLlm.mockReturnValue({ enabled: true, model: "claude", isLoading: false });
    const onToggle = vi.fn();
    const user = userEvent.setup();
    renderWithProviders(
      <ChatPanel
        parent={{ kind: "session", id: "s1" }}
        docked
        onDockedToggle={onToggle}
      />,
    );

    await user.click(screen.getByTestId("chat-panel-launcher"));
    expect(onToggle).toHaveBeenLastCalledWith(true);

    await user.click(screen.getByTestId("chat-panel-close"));
    expect(onToggle).toHaveBeenLastCalledWith(false);
  });
});

describe("ChatPanel — PR detection in chat bubbles", () => {
  const openChatWithPrMessage = async (user: ReturnType<typeof userEvent.setup>) => {
    mockedLlm.mockReturnValue({
      enabled: true,
      model: "claude-opus-4-5",
      isLoading: false,
    });
    vi.mocked(api.getChat).mockResolvedValue({
      chat: {
        id: "chat-1",
        session_id: "s1",
        project_id: null,
        owner_email: null,
        title: "Deploy questions",
        shared: false,
        created_at: "2026-06-06T00:00:00Z",
        updated_at: "2026-06-06T00:00:00Z",
      },
      messages: [
        {
          id: "m1",
          chat_id: "chat-1",
          role: "user",
          content: "Check https://github.com/acme/repo/pull/42 for the fix.",
          created_at: "2026-06-06T00:00:00Z",
        },
      ],
    });

    renderWithProviders(<ChatPanel parent={{ kind: "session", id: "s1" }} />);
    await user.click(await screen.findByText("Deploy questions"));
    // Wait for chat messages to load.
    await screen.findByText(/acme\/repo#42/);
  };

  it("renders a PR chip for a PR URL detected in a message", async () => {
    const user = userEvent.setup();
    await openChatWithPrMessage(user);
    expect(screen.getByTestId("pr-chip")).toBeInTheDocument();
  });

  it("shows a Link button for unlinked PRs in session-parented chats", async () => {
    const user = userEvent.setup();
    await openChatWithPrMessage(user);
    expect(screen.getByTestId("pr-link-button")).toBeInTheDocument();
  });

  it("calls api.linkSessionPr when the Link button is clicked", async () => {
    vi.mocked(api.linkSessionPr).mockResolvedValue({
      url: "https://github.com/acme/repo/pull/42",
      owner: "acme",
      repo: "repo",
      number: 42,
      status: null,
    });
    const user = userEvent.setup();
    await openChatWithPrMessage(user);

    await user.click(screen.getByTestId("pr-link-button"));

    await waitFor(() =>
      expect(api.linkSessionPr).toHaveBeenCalledWith(
        "s1",
        "https://github.com/acme/repo/pull/42",
      ),
    );
  });

  it("does not show a Link button when the PR is already linked", async () => {
    mockedLlm.mockReturnValue({
      enabled: true,
      model: "claude-opus-4-5",
      isLoading: false,
    });
    vi.mocked(api.listSessionPrs).mockResolvedValue([
      {
        url: "https://github.com/acme/repo/pull/42",
        owner: "acme",
        repo: "repo",
        number: 42,
        status: null,
      },
    ]);
    vi.mocked(api.getChat).mockResolvedValue({
      chat: {
        id: "chat-1",
        session_id: "s1",
        project_id: null,
        owner_email: null,
        title: "Deploy questions",
        shared: false,
        created_at: "2026-06-06T00:00:00Z",
        updated_at: "2026-06-06T00:00:00Z",
      },
      messages: [
        {
          id: "m1",
          chat_id: "chat-1",
          role: "user",
          content: "Check https://github.com/acme/repo/pull/42 for the fix.",
          created_at: "2026-06-06T00:00:00Z",
        },
      ],
    });

    const user = userEvent.setup();
    renderWithProviders(<ChatPanel parent={{ kind: "session", id: "s1" }} />);
    await user.click(await screen.findByText("Deploy questions"));
    await screen.findByText(/acme\/repo#42/);

    expect(screen.queryByTestId("pr-link-button")).not.toBeInTheDocument();
  });
});

describe("ChatPanel — model picker", () => {
  const openThreadView = async (user: ReturnType<typeof userEvent.setup>) => {
    mockedLlm.mockReturnValue({
      enabled: true,
      model: "claude-opus-4-5",
      isLoading: false,
    });
    vi.mocked(api.getChat).mockResolvedValue({
      chat: {
        id: "chat-1",
        session_id: "s1",
        project_id: null,
        owner_email: null,
        title: "Deploy questions",
        shared: false,
        created_at: "2026-06-06T00:00:00Z",
        updated_at: "2026-06-06T00:00:00Z",
      },
      messages: [],
    });
    renderWithProviders(<ChatPanel parent={{ kind: "session", id: "s1" }} />);
    await user.click(await screen.findByText("Deploy questions"));
    // Wait for thread view to be visible (Message input appears).
    await screen.findByLabelText("Message");
  };

  it("renders the model picker trigger in the thread view", async () => {
    const user = userEvent.setup();
    await openThreadView(user);
    expect(screen.getByTestId("model-picker-trigger")).toBeInTheDocument();
  });

  it("defaults the model picker to the LLM status model", async () => {
    const user = userEvent.setup();
    await openThreadView(user);
    // The trigger label should show the model name from the mock list.
    expect(screen.getByTestId("model-picker-trigger")).toHaveTextContent(
      "Claude Opus 4.5",
    );
  });

  it("opens the model combobox and shows filterable options", async () => {
    const user = userEvent.setup();
    await openThreadView(user);

    await user.click(screen.getByTestId("model-picker-trigger"));

    // The filter input must be present.
    expect(screen.getByLabelText("Filter models")).toBeInTheDocument();
    // Both models should appear (the list uses role=listbox).
    const list = screen.getByRole("listbox", { name: "Available models" });
    expect(list).toBeInTheDocument();
    expect(await screen.findAllByText("Claude Opus 4.5")).toHaveLength(2); // trigger + list
    expect(screen.getByText("Claude Sonnet 4.5")).toBeInTheDocument();
  });

  it("filters models by query text", async () => {
    const user = userEvent.setup();
    await openThreadView(user);

    await user.click(screen.getByTestId("model-picker-trigger"));
    const filterInput = screen.getByLabelText("Filter models");
    await user.type(filterInput, "sonnet");

    // After filtering, the Opus option should be gone from the list (not the trigger label).
    const list = screen.getByRole("listbox", { name: "Available models" });
    await waitFor(() =>
      expect(list.querySelector('[role="option"]')).toHaveTextContent(
        "Claude Sonnet 4.5",
      ),
    );
    // Opus should not appear as a list option anymore.
    const options = list.querySelectorAll('[role="option"]');
    expect(
      Array.from(options).some((o) => o.textContent?.includes("Opus")),
    ).toBe(false);
  });

  it("selecting a model from the picker updates the trigger label", async () => {
    const user = userEvent.setup();
    await openThreadView(user);

    await user.click(screen.getByTestId("model-picker-trigger"));
    await user.click(await screen.findByText("Claude Sonnet 4.5"));

    // Dropdown closes and trigger reflects the chosen model.
    expect(screen.getByTestId("model-picker-trigger")).toHaveTextContent(
      "Claude Sonnet 4.5",
    );
  });

  it("hides the model picker when llmModels returns empty (503-like)", async () => {
    vi.mocked(api.llmModels).mockResolvedValue([]);
    const user = userEvent.setup();
    await openThreadView(user);

    // Give the query time to resolve.
    await waitFor(() =>
      expect(screen.queryByTestId("model-picker-trigger")).not.toBeInTheDocument(),
    );
  });
});
