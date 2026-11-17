import { beforeEach, it, expect, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { AttentionDialog } from "./AttentionDialog";
import { Notifications } from "./Notifications";
import * as source from "@/lib/privateAttention";
import { api } from "@/lib/api";
vi.mock("@/lib/privateAttention", () => ({
  privateAttentionRecipients: vi.fn(),
  requestPrivateAttention: vi.fn(),
  privateNotifications: vi.fn(),
  privateNotificationAction: vi.fn(),
}));
const localState = vi.hoisted(() => ({ local: true, authenticated: true }));
vi.mock("@/lib/localHost", () => ({
  LOCAL_HOST_ID: "local",
  useLocalHost: () => localState,
}));
vi.mock("@/lib/hostPairing", () => ({
  usePairedHosts: () => ({ active: null, hosts: [] }),
}));
vi.mock("@/lib/useAuth", () => ({ useAuth: () => ({ user: null }) }));
vi.mock("@/lib/api", () => ({
  api: {
    notifications: vi.fn(),
    requestAttention: vi.fn(),
    attentionRecipients: vi.fn(),
    resolveNotification: vi.fn(),
  },
}));
const subject = { kind: "credential" as const, kid: "A".repeat(43) };
beforeEach(() => {
  vi.clearAllMocks();
  localState.authenticated = true;
});
it("uses approved identity labels and keeps the same idempotency key after ambiguous send", async () => {
  vi.mocked(source.privateAttentionRecipients).mockResolvedValue([
    { subject, label: "Laptop browser" },
  ]);
  vi.mocked(source.requestPrivateAttention)
    .mockRejectedValueOnce(new Error("Connection lost; delivery unknown"))
    .mockResolvedValueOnce(undefined);
  const close = vi.fn();
  const user = userEvent.setup();
  renderWithProviders(
    <AttentionDialog
      hostId="local"
      sessionId="s"
      title="Private work"
      onOpenChange={close}
    />,
  );
  await screen.findByRole("option", { name: "Laptop browser" });
  await user.type(screen.getByRole("textbox"), "Please check this");
  await user.click(screen.getByRole("button", { name: "Send request" }));
  await screen.findByText("Connection lost; delivery unknown");
  await user.click(screen.getByRole("button", { name: "Send request" }));
  await waitFor(() => expect(close).toHaveBeenCalledWith(false));
  const calls = vi.mocked(source.requestPrivateAttention).mock.calls;
  expect(calls[0]).toEqual([
    "local",
    "s",
    subject,
    "Please check this",
    expect.any(String),
  ]);
  expect(calls[1][4]).toBe(calls[0][4]);
  expect(api.requestAttention).not.toHaveBeenCalled();
});
it("shows local-owner inbox without a cloud user and resolves through its host", async () => {
  vi.mocked(source.privateNotifications).mockResolvedValue([
    {
      id: "n",
      session_id: "s",
      session_title: "Private work",
      kind: "attention",
      sender: { subject, label: "Approved browser" },
      recipient: { kind: "local_owner" },
      message: "Review please",
      created_at: "2026-11-05T00:00:00Z",
      read_at: null,
      resolved_at: null,
    },
  ]);
  vi.mocked(source.privateNotificationAction).mockResolvedValue(undefined);
  const user = userEvent.setup();
  renderWithProviders(<Notifications />);
  await user.click(
    await screen.findByRole("button", { name: /Notifications/ }),
  );
  expect(await screen.findByText("Approved browser")).toBeVisible();
  expect(screen.getByRole("link", { name: "Private work" })).toHaveAttribute(
    "href",
    "/private/s",
  );
  await user.click(screen.getByRole("button", { name: "Mark resolved" }));
  await waitFor(() =>
    expect(source.privateNotificationAction).toHaveBeenCalledWith(
      "local",
      "s",
      "n",
      true,
    ),
  );
  expect(api.notifications).not.toHaveBeenCalled();
  expect(api.resolveNotification).not.toHaveBeenCalled();
});

it("does not query inbox or recipients while the localhost owner is locked", async () => {
  localState.authenticated = false;
  renderWithProviders(
    <>
      <Notifications />
      <AttentionDialog
        hostId="local"
        sessionId="s"
        title="Private"
        onOpenChange={() => {}}
      />
    </>,
  );
  expect(screen.queryByRole("button", { name: /Notifications/ })).toBeNull();
  expect(source.privateNotifications).not.toHaveBeenCalled();
  expect(source.privateAttentionRecipients).not.toHaveBeenCalled();
  expect(api.notifications).not.toHaveBeenCalled();
  expect(api.attentionRecipients).not.toHaveBeenCalled();
});
