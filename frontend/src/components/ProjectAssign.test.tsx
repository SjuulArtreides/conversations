import { useState } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { ProjectAssign } from "./ProjectAssign";

vi.mock("@/lib/api", () => ({
  api: {
    projects: vi.fn(),
    createProject: vi.fn(),
    setSessionProject: vi.fn().mockResolvedValue({ project_id: null }),
  },
}));

import { api } from "@/lib/api";

const projects = [
  {
    id: "p1",
    name: "Atreides",
    description: null,
    owner_email: null,
    session_count: 1,
    created_at: "2026-06-06T00:00:00Z",
    updated_at: "2026-06-06T00:00:00Z",
  },
  {
    id: "p2",
    name: "Harkonnen",
    description: null,
    owner_email: null,
    session_count: 0,
    created_at: "2026-06-06T00:00:00Z",
    updated_at: "2026-06-06T00:00:00Z",
  },
];

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(api.projects).mockResolvedValue(projects);
});

describe("ProjectAssign combobox", () => {
  it("filters to a matching project and assigns it on click", async () => {
    const user = userEvent.setup();
    renderWithProviders(<ProjectAssign sessionId="s1" projectId={null} />);

    const input = await screen.findByLabelText("Assign to project");
    await user.click(input);
    await user.type(input, "Hark");

    // Filtered: Harkonnen visible, Atreides hidden.
    const option = await screen.findByRole("button", { name: "Harkonnen" });
    expect(
      screen.queryByRole("button", { name: "Atreides" }),
    ).not.toBeInTheDocument();

    await user.click(option);
    expect(api.setSessionProject).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(api.setSessionProject).toHaveBeenCalledWith("s1", "p2"),
    );
    expect(api.createProject).not.toHaveBeenCalled();
  });

  it("typing an exact existing name + Enter assigns it (no create)", async () => {
    const user = userEvent.setup();
    renderWithProviders(<ProjectAssign sessionId="s1" projectId={null} />);

    const input = await screen.findByLabelText("Assign to project");
    await user.click(input);
    await user.type(input, "Atreides{Enter}");

    expect(api.setSessionProject).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(api.setSessionProject).toHaveBeenCalledWith("s1", "p1"),
    );
    expect(api.createProject).not.toHaveBeenCalled();
  });

  it("typing a NEW name + Enter confirms create, then creates + assigns", async () => {
    vi.mocked(api.createProject).mockResolvedValue({
      id: "p3",
      name: "Corrino",
      description: null,
      owner_email: null,
      session_count: 0,
      created_at: "2026-06-06T00:00:00Z",
      updated_at: "2026-06-06T00:00:00Z",
    });
    vi.mocked(api.setSessionProject).mockResolvedValue({ project_id: "p3" });

    const user = userEvent.setup();
    renderWithProviders(<ProjectAssign sessionId="s1" projectId={null} />);

    const input = await screen.findByLabelText("Assign to project");
    await user.click(input);
    await user.type(input, "Corrino{Enter}");

    // Confirm dialog appears.
    expect(await screen.findByText("Create project?")).toBeInTheDocument();
    expect(api.createProject).not.toHaveBeenCalled();

    await user.click(
      screen.getByRole("button", { name: "Select new project" }),
    );

    expect(api.createProject).not.toHaveBeenCalled();
    expect(api.setSessionProject).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(api.createProject).toHaveBeenCalledWith("Corrino"),
    );
    await waitFor(() =>
      expect(api.setSessionProject).toHaveBeenCalledWith("s1", "p3"),
    );
  });

  it("clears the assignment via the No project affordance", async () => {
    const user = userEvent.setup();
    renderWithProviders(<ProjectAssign sessionId="s1" projectId="p1" />);

    const input = await screen.findByLabelText("Assign to project");
    await user.click(input);
    await user.click(await screen.findByRole("button", { name: "No project" }));

    expect(api.setSessionProject).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(api.setSessionProject).toHaveBeenCalledWith("s1", null),
    );
  });
  it("follows later assignments after saving, while preserving an unsaved choice", async () => {
    const user = userEvent.setup();
    function Harness() {
      const [project, setProject] = useState<string | null>(null);
      return (
        <>
          <ProjectAssign sessionId="s1" projectId={project} />
          <button onClick={() => setProject("p1")}>External Atreides</button>
          <button onClick={() => setProject("p2")}>External Harkonnen</button>
        </>
      );
    }
    renderWithProviders(<Harness />);
    const input = await screen.findByLabelText("Assign to project");
    await user.click(input);
    await user.click(await screen.findByRole("button", { name: "Harkonnen" }));
    await user.click(screen.getByRole("button", { name: "External Atreides" }));
    expect(input).toHaveAttribute("placeholder", "Harkonnen");
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(api.setSessionProject).toHaveBeenCalledWith("s1", "p2"),
    );
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Save" })).toBeDisabled(),
    );
    await user.click(
      screen.getByRole("button", { name: "External Harkonnen" }),
    );
    expect(input).toHaveAttribute("placeholder", "Harkonnen");
    await user.click(screen.getByRole("button", { name: "External Atreides" }));
    expect(input).toHaveAttribute("placeholder", "Atreides");
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
  });
});
