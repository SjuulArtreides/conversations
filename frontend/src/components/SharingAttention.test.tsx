import { beforeEach, expect, it, vi } from "vitest";
import { fireEvent, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { api } from "@/lib/api";
import type { SessionNotification, LiveSession } from "@/lib/types";
import { SessionContext } from "./SessionContext";
import { LiveTabs } from "./LiveTabs";
import { Notifications } from "./Notifications";
import { ShareDialog } from "./ShareDialog";
import { LiveShareDialog } from "./LiveShareDialog";

vi.mock("./HostActions", () => ({
  useRestartSession: () => ({ available: false }),
  useEndLiveSession: () => ({}),
  useDismissLiveSession: () => ({}),
}));
vi.mock("@/lib/useAuth", () => ({
  useAuth: () => ({ user: { email: "recipient@example.com" } }),
}));
vi.mock("@/lib/api", () => ({
  api: {
    notifications: vi.fn(),
    readNotification: vi.fn(),
    resolveNotification: vi.fn(),
    attentionRecipients: vi.fn(),
    requestAttention: vi.fn(),
    getLiveSharing: vi.fn(),
    setLiveSharing: vi.fn(),
    setSharing: vi.fn(),
    searchUsers: vi.fn(),
  },
}));

let notifications: SessionNotification[];
const base: SessionNotification = {
  id: "share",
  session_id: "s1",
  kind: "share",
  sender_email: "owner@example.com",
  recipient_email: "recipient@example.com",
  message: "Review the tenant setup",
  created_at: "2026-10-23T10:00:00Z",
  read_at: null,
  resolved_at: null,
  session_title: "Tenant setup",
};
const session: LiveSession = {
  id: "s1",
  session_id: "s1",
  title: "Tenant setup",
  project_path: null,
  started_at: null,
  last_event_at: null,
  mode: "interactive",
  viewer_is_author: false,
  viewer_can_write: false,
};
beforeEach(() => {
  vi.clearAllMocks();
  notifications = [{ ...base }];
  vi.mocked(api.notifications).mockImplementation(async () => [
    ...notifications,
  ]);
  vi.mocked(api.readNotification).mockImplementation(async (id) => {
    notifications = notifications.map((item) =>
      item.id === id ? { ...item, read_at: "now" } : item,
    );
    return { ok: true };
  });
  vi.mocked(api.resolveNotification).mockImplementation(async (id) => {
    notifications = notifications.map((item) =>
      item.id === id ? { ...item, resolved_at: "now", read_at: "now" } : item,
    );
    return { ok: true };
  });
  vi.mocked(api.attentionRecipients).mockResolvedValue([
    { email: "owner@example.com", display_name: "Owner" },
  ]);
  vi.mocked(api.requestAttention).mockResolvedValue({ ok: true });
  vi.mocked(api.getLiveSharing).mockResolvedValue([]);
  vi.mocked(api.searchUsers).mockResolvedValue([]);
  vi.mocked(api.setSharing).mockResolvedValue({
    visibility: "restricted",
    emails: ["recipient@example.com"],
  });
  vi.mocked(api.setLiveSharing).mockResolvedValue([
    { email: "recipient@example.com", can_write: false },
  ]);
});

it("shows sharing context on tap and provides the reason to keyboard users", async () => {
  const user = userEvent.setup();
  renderWithProviders(<SessionContext sessionId="s1" />);
  const icon = await screen.findByRole("button", { name: "Share reason" });
  fireEvent.focus(icon);
  expect(screen.getByRole("tooltip")).toHaveTextContent(
    "owner@example.com: Review the tenant setup",
  );
  await user.click(icon);
  expect(icon).toHaveAttribute("aria-expanded", "true");
  expect(screen.getByRole("tooltip")).toBeVisible();
  await user.keyboard("{Escape}");
  expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
});

it("does not show a reason icon for empty sharing context", async () => {
  notifications = [{ ...base, message: "" }];
  renderWithProviders(<SessionContext sessionId="s1" />);
  await waitFor(() => expect(api.notifications).toHaveBeenCalled());
  expect(
    screen.queryByRole("button", { name: "Share reason" }),
  ).not.toBeInTheDocument();
});

it("keeps the tab red after reading, until every attention request is resolved", async () => {
  notifications = [1, 2].map((id) => ({
    ...base,
    id: String(id),
    kind: "attention",
    message: `Question ${id}`,
  }));
  const user = userEvent.setup();
  renderWithProviders(
    <>
      <LiveTabs
        sessions={[session, { ...session, id: "s2", title: "Other" }]}
        currentId="s1"
      />
      <Notifications />
    </>,
  );
  const tab = screen.getByRole("tab", { name: /Tenant setup/ });
  await waitFor(() => expect(tab.parentElement).toHaveClass("bg-red-500/20"));
  expect(tab).toHaveAttribute(
    "title",
    "owner@example.com: Question 1\n\nowner@example.com: Question 2",
  );
  await user.click(screen.getByRole("button", { name: /Notifications/ }));
  await user.click(screen.getAllByRole("button", { name: "Mark read" })[0]);
  await waitFor(() =>
    expect(screen.getAllByRole("button", { name: "Mark read" })).toHaveLength(
      1,
    ),
  );
  expect(tab.parentElement).toHaveClass("bg-red-500/20");
  await user.click(screen.getByRole("button", { name: "Close" }));
  await user.click(screen.getByRole("button", { name: "Attention request" }));
  await user.click(screen.getAllByRole("button", { name: "Mark resolved" })[0]);
  await waitFor(() =>
    expect(
      screen.getAllByRole("button", { name: "Mark resolved" }),
    ).toHaveLength(1),
  );
  expect(tab.parentElement).toHaveClass("bg-red-500/20");
  await user.click(screen.getByRole("button", { name: "Mark resolved" }));
  await waitFor(() =>
    expect(tab.parentElement).not.toHaveClass("bg-red-500/20"),
  );
});

it("lets a read-only colleague request attention without terminal actions", async () => {
  const user = userEvent.setup();
  renderWithProviders(
    <LiveTabs
      sessions={[session, { ...session, id: "s2", title: "Other" }]}
      currentId="s1"
    />,
  );
  fireEvent.contextMenu(screen.getByRole("tab", { name: /Tenant setup/ }));
  expect(
    screen.queryByRole("menuitem", { name: /Resize|Restart|End session/ }),
  ).not.toBeInTheDocument();
  await user.click(
    screen.getByRole("menuitem", { name: "Ask for attention…" }),
  );
  const send = await screen.findByRole("button", { name: "Send request" });
  expect(send).toBeDisabled();
  await user.type(
    screen.getByLabelText("What do you need help with?"),
    "Can you check tenant isolation?",
  );
  await waitFor(() => expect(send).toBeEnabled());
  await user.click(send);
  await waitFor(() =>
    expect(api.requestAttention).toHaveBeenCalledWith(
      "s1",
      "owner@example.com",
      "Can you check tenant isolation?",
    ),
  );
});

it.each([false, true])(
  "submits the reason from stored/live sharing (live=%s)",
  async (live) => {
    const user = userEvent.setup();
    renderWithProviders(
      live ? (
        <LiveShareDialog
          liveId="s1"
          title="Tenant setup"
          open
          onOpenChange={() => {}}
        />
      ) : (
        <ShareDialog
          session={{
            id: "s1",
            title: "Tenant setup",
            visibility: "restricted",
            shared_emails: ["recipient@example.com"],
          }}
          open
          onOpenChange={() => {}}
        />
      ),
    );
    await user.type(
      screen.getByLabelText("Why are you sharing this?"),
      "Please review",
    );
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      live
        ? expect(api.setLiveSharing).toHaveBeenCalledWith(
            "s1",
            [],
            "Please review",
          )
        : expect(api.setSharing).toHaveBeenCalledWith("s1", {
            visibility: "restricted",
            emails: ["recipient@example.com"],
            reason: "Please review",
          }),
    );
  },
);
