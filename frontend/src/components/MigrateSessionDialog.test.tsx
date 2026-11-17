import { beforeEach, expect, it, vi } from "vitest";
import { act, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { migrateSession, MigrationResumeError } from "@/lib/sessionMigration";
import { api } from "@/lib/api";
import { isLocalHost, localRequest } from "@/lib/localHost";
import { MigrateSessionDialog } from "./MigrateSessionDialog";

vi.mock("@/lib/hostPairing", () => ({
  isPinned: (host: { key?: string }) => !!host.key,
  usePairedHosts: () => ({
    hosts: [
      { id: "source", label: "Laptop", key: "pinned" },
      { id: "target", label: "Workstation", key: "pinned" },
      { id: "legacy", label: "Unverified host" },
    ],
  }),
}));
vi.mock("@/lib/sessionMigration", async (original) => ({
  ...(await original<typeof import("@/lib/sessionMigration")>()),
  migrateSession: vi.fn(),
}));
vi.mock("@/lib/api", () => ({ api: { getUserSettings: vi.fn() } }));
vi.mock("@/lib/localHost", () => ({
  LOCAL_HOST_ID: "local",
  isLocalHost: vi.fn(() => false),
  localRequest: vi.fn(),
}));
beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(isLocalHost).mockReturnValue(false);
  vi.mocked(api.getUserSettings).mockResolvedValue({ container_paths: [] });
  vi.mocked(localRequest).mockResolvedValue({ container_paths: [] });
});
const session = {
  id: "session",
  agent: "codex",
  cwd: "/old/project",
  title: "Fix",
  messages: 2,
  last_activity: null,
  running: true,
};
it("requires another pinned host and sends the chosen destination folder", async () => {
  vi.mocked(migrateSession).mockResolvedValue("copied");
  const close = vi.fn();
  renderWithProviders(
    <MigrateSessionDialog
      session={session}
      hostId="source"
      onOpenChange={close}
    />,
  );
  expect(
    screen.queryByRole("option", { name: "Laptop" }),
  ).not.toBeInTheDocument();
  expect(
    screen.queryByRole("option", { name: "Unverified host" }),
  ).not.toBeInTheDocument();
  expect(
    screen.getByRole("button", { name: "Copy and resume" }),
  ).toBeDisabled();
  await userEvent.selectOptions(
    screen.getByLabelText("Destination host"),
    "target",
  );
  await userEvent.clear(screen.getByLabelText("Folder path"));
  await userEvent.type(screen.getByLabelText("Folder path"), "/new/project");
  await userEvent.click(
    screen.getByRole("button", { name: "Copy and resume" }),
  );
  await waitFor(() => expect(close).toHaveBeenCalledWith(false));
  expect(migrateSession).toHaveBeenCalledWith(
    expect.objectContaining({
      sourceHost: "source",
      sourceSession: "session",
      destinationHost: "target",
      cwd: "/new/project",
    }),
  );
});
it("shows a link to the retained copy when resume fails and prevents another copy", async () => {
  vi.mocked(migrateSession).mockRejectedValue(
    new MigrationResumeError("saved", new Error("offline")),
  );
  renderWithProviders(
    <MigrateSessionDialog
      session={session}
      hostId="source"
      onOpenChange={vi.fn()}
    />,
  );
  await userEvent.selectOptions(
    screen.getByLabelText("Destination host"),
    "target",
  );
  await userEvent.click(
    screen.getByRole("button", { name: "Copy and resume" }),
  );
  expect(await screen.findByRole("alert")).toHaveTextContent("was copied");
  expect(
    screen.getByRole("link", { name: "Open destination session" }),
  ).toHaveAttribute("href", "/private/saved?host=target");
  expect(
    screen.getByRole("button", { name: "Copy and resume" }),
  ).toBeDisabled();
});

it.each([false, true])(
  "uses saved repositories from the correct settings source (local: %s)",
  async (local) => {
    vi.mocked(isLocalHost).mockReturnValue(local);
    vi.mocked(api.getUserSettings).mockResolvedValue({
      container_paths: ["/hosted/repo"],
    });
    vi.mocked(localRequest).mockResolvedValue({
      container_paths: ["/local/repo"],
    });
    vi.mocked(migrateSession).mockResolvedValue("copied");
    const { queryClient } = renderWithProviders(
      <MigrateSessionDialog
        session={session}
        hostId="source"
        onOpenChange={vi.fn()}
      />,
    );
    const path = local ? "/local/repo" : "/hosted/repo";
    await screen.findByRole("option", { name: path });
    if (local) {
      expect(localRequest).toHaveBeenCalledWith("/api/local/preferences");
      expect(api.getUserSettings).not.toHaveBeenCalled();
    } else {
      expect(api.getUserSettings).toHaveBeenCalledOnce();
      expect(localRequest).not.toHaveBeenCalled();
    }
    expect(
      queryClient.getQueryData([
        local ? "local-user-settings" : "user-settings",
      ]),
    ).toEqual({ container_paths: [path] });
    expect(screen.getByLabelText("Folder path")).toHaveValue(session.cwd);
    await userEvent.selectOptions(
      screen.getByLabelText("Destination folder"),
      path,
    );
    await userEvent.selectOptions(
      screen.getByLabelText("Destination host"),
      "target",
    );
    await userEvent.click(
      screen.getByRole("button", { name: "Copy and resume" }),
    );
    expect(migrateSession).toHaveBeenCalledWith(
      expect.objectContaining({ cwd: path }),
    );
  },
);

it("allows a custom folder after choosing a saved repository", async () => {
  vi.mocked(api.getUserSettings).mockResolvedValue({
    container_paths: ["/saved/repo"],
  });
  vi.mocked(migrateSession).mockResolvedValue("copied");
  renderWithProviders(
    <MigrateSessionDialog
      session={session}
      hostId="source"
      onOpenChange={vi.fn()}
    />,
  );
  await screen.findByRole("option", { name: "/saved/repo" });
  await userEvent.selectOptions(
    screen.getByLabelText("Destination folder"),
    "/saved/repo",
  );
  await userEvent.selectOptions(
    screen.getByLabelText("Destination folder"),
    "",
  );
  await userEvent.clear(screen.getByLabelText("Folder path"));
  await userEvent.type(screen.getByLabelText("Folder path"), "/custom/repo");
  await userEvent.selectOptions(
    screen.getByLabelText("Destination host"),
    "target",
  );
  await userEvent.click(
    screen.getByRole("button", { name: "Copy and resume" }),
  );
  expect(migrateSession).toHaveBeenCalledWith(
    expect.objectContaining({ cwd: "/custom/repo" }),
  );
});

it.each([false, true])(
  "keeps custom folders usable with missing settings (error: %s)",
  async (error) => {
    if (error)
      vi.mocked(api.getUserSettings).mockRejectedValue(new Error("offline"));
    vi.mocked(migrateSession).mockResolvedValue("copied");
    renderWithProviders(
      <MigrateSessionDialog
        session={session}
        hostId="source"
        onOpenChange={vi.fn()}
      />,
    );
    if (error) await screen.findByText(/Could not load saved repositories/);
    await userEvent.selectOptions(
      screen.getByLabelText("Destination host"),
      "target",
    );
    await userEvent.click(
      screen.getByRole("button", { name: "Copy and resume" }),
    );
    expect(migrateSession).toHaveBeenCalledWith(
      expect.objectContaining({ cwd: session.cwd }),
    );
  },
);

it("does not overwrite an edited folder when settings arrive and locks it during copying", async () => {
  let resolveSettings!: (settings: { container_paths: string[] }) => void;
  vi.mocked(api.getUserSettings).mockReturnValue(
    new Promise((resolve) => {
      resolveSettings = resolve;
    }),
  );
  vi.mocked(migrateSession).mockReturnValue(new Promise(() => {}));
  renderWithProviders(
    <MigrateSessionDialog
      session={session}
      hostId="source"
      onOpenChange={vi.fn()}
    />,
  );
  await userEvent.clear(screen.getByLabelText("Folder path"));
  await userEvent.type(screen.getByLabelText("Folder path"), "/typed/repo");
  await act(async () => resolveSettings({ container_paths: ["/saved/repo"] }));
  await screen.findByRole("option", { name: "/saved/repo" });
  expect(screen.getByLabelText("Folder path")).toHaveValue("/typed/repo");
  await userEvent.selectOptions(
    screen.getByLabelText("Destination host"),
    "target",
  );
  await userEvent.click(
    screen.getByRole("button", { name: "Copy and resume" }),
  );
  expect(screen.getByLabelText("Destination folder")).toBeDisabled();
  expect(screen.getByLabelText("Folder path")).toBeDisabled();
  expect(screen.getByLabelText("Destination host")).toBeDisabled();
  expect(migrateSession).toHaveBeenCalledWith(
    expect.objectContaining({ cwd: "/typed/repo" }),
  );
});
