import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Route, Routes } from "react-router-dom";
import { renderWithProviders } from "@/test/utils";
import { ProjectDetail } from "./ProjectDetail";

vi.mock("@/lib/api", async () => {
  const actual = await vi.importActual<typeof import("@/lib/api")>("@/lib/api");
  return {
    ...actual,
    api: {
      authConfig: vi.fn().mockResolvedValue(null),
      getProject: vi.fn(),
      updateProject: vi.fn().mockResolvedValue({}),
      deleteProject: vi.fn(),
    },
  };
});

// Keep the embedded ChatPanel inert.
vi.mock("@/lib/useLlmStatus", () => ({
  useLlmStatus: () => ({ enabled: false, model: null, isLoading: false }),
}));

import { api } from "@/lib/api";

const projectDetail = {
  project: {
    id: "p1",
    name: "Atreides",
    description: "House data and logistics",
    owner_email: null,
    session_count: 0,
    created_at: "2026-06-06T00:00:00Z",
    updated_at: "2026-06-06T00:00:00Z",
  },
  sessions: [],
};

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(api.getProject).mockResolvedValue(projectDetail);
});

function renderDetail() {
  return renderWithProviders(
    <Routes>
      <Route path="/projects/:id" element={<ProjectDetail />} />
    </Routes>,
    { route: "/projects/p1" },
  );
}

describe("ProjectDetail description", () => {
  it("renders the project description", async () => {
    renderDetail();
    expect(
      await screen.findByText("House data and logistics"),
    ).toBeInTheDocument();
  });

  it("edits name + description and PUTs both", async () => {
    const user = userEvent.setup();
    renderDetail();

    await user.click(await screen.findByRole("button", { name: "Edit" }));

    const desc = screen.getByLabelText("Project description");
    await user.clear(desc);
    await user.type(desc, "Updated charter");
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() =>
      expect(api.updateProject).toHaveBeenCalledWith("p1", {
        name: "Atreides",
        description: "Updated charter",
      }),
    );
  });
});
