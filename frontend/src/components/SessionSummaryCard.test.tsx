import { afterEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import type { Session, SessionSummary } from "@/lib/types";
import { SessionSummaryCard } from "./SessionSummaryCard";

// Mock the LLM-status hook so tests drive the enabled/disabled branch directly
// without touching the network.
const mockUseLlmStatus = vi.fn();
vi.mock("@/lib/useLlmStatus", () => ({
  useLlmStatus: () => mockUseLlmStatus(),
}));

// Mock the API so we can hold the generate mutation in a pending state.
const mockGenerateSummary = vi.fn();
vi.mock("@/lib/api", () => ({
  api: {
    generateSummary: (id: string) => mockGenerateSummary(id),
  },
}));

function makeSession(summary: SessionSummary | null): Session {
  return {
    id: "s1",
    title: "Test session",
    project_path: null,
    git_branch: null,
    message_count: 0,
    started_at: null,
    ended_at: null,
    uploaded_by: "alice@example.com",
    uploaded_at: "2026-06-06T00:00:00Z",
    source_host: null,
    owner_email: null,
    visibility: "organization",
    repos: [],
    cwd: null,
    claude_version: null,
    skipped_lines: 0,
    shared_emails: [],
    messages: [],
    summary,
    project_id: null,
  };
}

const populated: SessionSummary = {
  intent: "Fix the flaky login test",
  invariants: ["Tokens never logged", "Auth is default-closed"],
  summary: "We patched the **retry** logic and added a guard.",
  model: "claude-opus-4",
  generated_by: "bob@example.com",
  generated_at: "2026-06-06T00:00:00Z",
};

describe("SessionSummaryCard", () => {
  afterEach(() => {
    mockUseLlmStatus.mockReset();
    mockGenerateSummary.mockReset();
  });

  it("renders nothing when the LLM is disabled", () => {
    mockUseLlmStatus.mockReturnValue({
      enabled: false,
      model: null,
      isLoading: false,
    });
    renderWithProviders(
      <SessionSummaryCard session={makeSession(populated)} />,
    );
    // The card renders nothing — none of its content is present.
    expect(screen.queryByText("Summary")).not.toBeInTheDocument();
    expect(screen.queryByText("Fix the flaky login test")).not.toBeInTheDocument();
    expect(screen.queryByRole("button")).not.toBeInTheDocument();
  });

  it("shows the empty state and a Generate button when enabled with no summary", () => {
    mockUseLlmStatus.mockReturnValue({
      enabled: true,
      model: "claude-opus-4",
      isLoading: false,
    });
    renderWithProviders(<SessionSummaryCard session={makeSession(null)} />);
    expect(screen.getByText(/No summary yet/i)).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Generate" }),
    ).toBeInTheDocument();
  });

  it("renders intent, invariants, summary, and a Regenerate button when populated", () => {
    mockUseLlmStatus.mockReturnValue({
      enabled: true,
      model: "claude-opus-4",
      isLoading: false,
    });
    renderWithProviders(
      <SessionSummaryCard session={makeSession(populated)} />,
    );
    expect(screen.getByText("Fix the flaky login test")).toBeInTheDocument();
    expect(screen.getByText("Tokens never logged")).toBeInTheDocument();
    expect(screen.getByText("Auth is default-closed")).toBeInTheDocument();
    // Summary prose renders through <Markdown>; the bold word stays.
    expect(screen.getByText("retry").tagName.toLowerCase()).toBe("strong");
    expect(
      screen.getByRole("button", { name: "Regenerate" }),
    ).toBeInTheDocument();
  });

  it("shows a progress indicator in the card body while generation is pending", async () => {
    mockUseLlmStatus.mockReturnValue({
      enabled: true,
      model: "claude-opus-4",
      isLoading: false,
    });
    // Never-resolving promise keeps the mutation pending.
    mockGenerateSummary.mockReturnValue(new Promise(() => {}));

    const user = userEvent.setup();
    renderWithProviders(<SessionSummaryCard session={makeSession(null)} />);

    await user.click(screen.getByRole("button", { name: "Generate" }));

    // The empty-state copy is replaced by a live progress indicator.
    await waitFor(() => {
      expect(screen.getByText(/Generating summary/i)).toBeInTheDocument();
    });
    expect(screen.getByRole("status")).toBeInTheDocument();
    expect(screen.queryByText(/No summary yet/i)).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Generating…" }),
    ).toBeInTheDocument();
  });
});
