import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { ProjectList } from "./ProjectList";

vi.mock("@/lib/api", async () => {
  const actual = await vi.importActual<typeof import("@/lib/api")>("@/lib/api");
  return {
    ...actual,
    api: {
      projects: vi.fn().mockResolvedValue([]),
      createProject: vi.fn().mockResolvedValue({
        id: "p1",
        name: "Atreides",
        description: "House data",
        owner_email: null,
        session_count: 0,
        created_at: "2026-06-06T00:00:00Z",
        updated_at: "2026-06-06T00:00:00Z",
      }),
    },
  };
});

import { api } from "@/lib/api";

beforeEach(() => {
  vi.clearAllMocks();
});

describe("ProjectList create form", () => {
  it("sends name + description to createProject", async () => {
    const user = userEvent.setup();
    renderWithProviders(<ProjectList />);

    await user.type(screen.getByLabelText("New project name"), "Atreides");
    await user.type(
      screen.getByLabelText("New project description"),
      "House data",
    );
    await user.click(screen.getByRole("button", { name: "Create project" }));

    await waitFor(() =>
      expect(api.createProject).toHaveBeenCalledWith("Atreides", "House data"),
    );
  });

  it("omits an empty description (sends undefined)", async () => {
    const user = userEvent.setup();
    renderWithProviders(<ProjectList />);

    await user.type(screen.getByLabelText("New project name"), "Harkonnen");
    await user.click(screen.getByRole("button", { name: "Create project" }));

    await waitFor(() =>
      expect(api.createProject).toHaveBeenCalledWith("Harkonnen", undefined),
    );
  });
});
