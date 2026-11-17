import { beforeEach, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { SessionActionsMenu } from "./SessionActionsMenu";
import { api } from "@/lib/api";

vi.mock("./HostActions", () => ({
  useRestartSession: () => ({ available: false }),
  useEndLiveSession: () => ({}),
}));
vi.mock("@/lib/api", () => ({
  api: {
    getSession: vi.fn(),
    projects: vi.fn(),
    setSessionProject: vi.fn(),
  },
}));

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(api.getSession).mockResolvedValue({ project_id: null } as Awaited<
    ReturnType<typeof api.getSession>
  >);
  vi.mocked(api.projects).mockResolvedValue([
    { id: "p1", name: "My project" },
  ] as Awaited<ReturnType<typeof api.projects>>);
  vi.mocked(api.setSessionProject).mockResolvedValue({ project_id: "p1" });
});

it("assigns the selected session from the Project menu", async () => {
  const user = userEvent.setup();
  renderWithProviders(
    <SessionActionsMenu
      session={{
        id: "s1",
        title: "Session",
        viewer_is_author: true,
        viewer_can_write: true,
      }}
      sessions={[]}
      currentId="s1"
    />,
  );
  await user.click(screen.getByRole("button", { name: "Actions" }));
  await user.click(screen.getByRole("menuitem", { name: "Project" }));
  await user.click(await screen.findByLabelText("Assign to project"));
  await user.click(await screen.findByRole("button", { name: "My project" }));
  expect(api.setSessionProject).not.toHaveBeenCalled();
  await user.click(screen.getByRole("button", { name: "Save" }));
  await waitFor(() =>
    expect(api.setSessionProject).toHaveBeenCalledWith("s1", "p1"),
  );
});

it("discards a project selection when cancelled", async () => {
  const user = userEvent.setup();
  renderWithProviders(
    <SessionActionsMenu
      session={{
        id: "s1",
        title: "Session",
        viewer_is_author: true,
        viewer_can_write: true,
      }}
      sessions={[]}
      currentId="s1"
    />,
  );
  await user.click(screen.getByRole("button", { name: "Actions" }));
  await user.click(screen.getByRole("menuitem", { name: "Project" }));
  await user.click(await screen.findByLabelText("Assign to project"));
  await user.click(await screen.findByRole("button", { name: "My project" }));
  await user.click(screen.getByRole("button", { name: "Cancel" }));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(api.setSessionProject).not.toHaveBeenCalled();
  await user.click(screen.getByRole("button", { name: "Actions" }));
  await user.click(screen.getByRole("menuitem", { name: "Project" }));
  expect(await screen.findByLabelText("Assign to project")).toHaveAttribute(
    "placeholder",
    "No project",
  );
});
