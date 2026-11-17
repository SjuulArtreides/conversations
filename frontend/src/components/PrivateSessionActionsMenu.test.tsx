import { privateProjects } from "@/lib/privateProjects";
import { api } from "@/lib/api";
import { beforeEach, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { PrivateSessionActionsMenu } from "./PrivateSessionActionsMenu";

it("opens the migration dialog from the session Actions menu", async () => {
  renderWithProviders(
    <PrivateSessionActionsMenu session={session} hostId="host-a" />,
  );
  await userEvent.click(screen.getByRole("button", { name: "Actions" }));
  await userEvent.click(screen.getByRole("menuitem", { name: "Migrate…" }));
  expect(
    await screen.findByRole("dialog", { name: "Migrate session" }),
  ).toBeInTheDocument();
  expect(screen.getByLabelText("Destination host")).toBeInTheDocument();
  expect(hostControl.endSession).not.toHaveBeenCalled();
});
import { hostControl } from "@/lib/hostControl";
vi.mock("@/lib/hostControl", () => ({
  hostControl: {
    endSession: vi.fn(),
    reopenSession: vi.fn(),
    renameSession: vi.fn(),
  },
}));
const session = {
  id: "private-id",
  title: "Private task",
  agent: "codex",
  cwd: "/work",
  running: true,
  messages: 1,
  last_activity: null,
};
beforeEach(() => vi.clearAllMocks());
it("ends the real host session through the shared Actions menu", async () => {
  vi.mocked(hostControl.endSession).mockResolvedValue();
  renderWithProviders(
    <PrivateSessionActionsMenu session={session} hostId="host-a" />,
  );
  await userEvent.click(screen.getByRole("button", { name: "Actions" }));
  await userEvent.click(screen.getByRole("menuitem", { name: "End session" }));
  await waitFor(() =>
    expect(hostControl.endSession).toHaveBeenCalledWith("private-id", "host-a"),
  );
  expect(hostControl.reopenSession).not.toHaveBeenCalled();
});
it("reopens a stopped private session and retains the selected host", async () => {
  vi.mocked(hostControl.reopenSession).mockResolvedValue({ delivered: true });
  renderWithProviders(
    <PrivateSessionActionsMenu
      session={{ ...session, running: false }}
      hostId="host-b"
    />,
  );
  await userEvent.click(screen.getByRole("button", { name: "Actions" }));
  await userEvent.click(
    screen.getByRole("menuitem", { name: "Re-open as live" }),
  );
  await waitFor(() =>
    expect(hostControl.reopenSession).toHaveBeenCalledWith(
      "private-id",
      "host-b",
    ),
  );
});
it("renames through host authority using the same rename dialog", async () => {
  vi.mocked(hostControl.renameSession).mockResolvedValue("Updated");
  renderWithProviders(
    <PrivateSessionActionsMenu session={session} hostId="host-a" />,
  );
  await userEvent.click(screen.getByRole("button", { name: "Actions" }));
  await userEvent.click(
    screen.getByRole("menuitem", { name: "Rename session" }),
  );
  await userEvent.clear(screen.getByLabelText("Session title"));
  await userEvent.type(screen.getByLabelText("Session title"), "Updated");
  await userEvent.click(screen.getByRole("button", { name: "Save" }));
  await waitFor(() =>
    expect(hostControl.renameSession).toHaveBeenCalledWith(
      "private-id",
      "Updated",
      "host-a",
    ),
  );
});

vi.mock("@/lib/privateProjects", () => ({
  privateProjects: { membership: vi.fn(), list: vi.fn(), assign: vi.fn() },
}));
vi.mock("@/lib/api", () => ({
  api: { getSession: vi.fn(), listProjects: vi.fn(), assignProject: vi.fn() },
}));
it("opens project assignment from the private menu through host authority only", async () => {
  vi.mocked(privateProjects.membership).mockResolvedValue({ project_id: null });
  vi.mocked(privateProjects.list).mockResolvedValue([]);
  renderWithProviders(
    <PrivateSessionActionsMenu session={session} hostId="host-a" />,
  );
  await userEvent.click(screen.getByRole("button", { name: "Actions" }));
  await userEvent.click(screen.getByRole("menuitem", { name: "Project" }));
  await screen.findByRole("dialog", { name: "Session project" });
  await waitFor(() =>
    expect(privateProjects.membership).toHaveBeenCalledWith(
      "host-a",
      "private-id",
    ),
  );
  await waitFor(() =>
    expect(privateProjects.list).toHaveBeenCalledWith("host-a"),
  );
  expect(api.getSession).not.toHaveBeenCalled();
  expect(api.listProjects).not.toHaveBeenCalled();
});
