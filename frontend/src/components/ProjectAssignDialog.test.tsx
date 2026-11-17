import { useState } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import { ToastProvider } from "./ui/toast";
import { ProjectAssignDialog } from "./ProjectAssignDialog";
import { api } from "@/lib/api";
import { localRequest } from "@/lib/localHost";
vi.mock("@/lib/localHost", () => ({
  isLocalHost: () => true,
  localRequest: vi.fn(),
}));
vi.mock("@/lib/api", () => ({
  api: { projects: vi.fn(), setSessionProject: vi.fn(), getSession: vi.fn() },
}));
it("refreshes owner membership after save before the same dialog reopens", async () => {
  let projectId = "p1";
  vi.mocked(localRequest).mockImplementation(
    async () => ({ project_id: projectId }) as never,
  );
  vi.mocked(api.projects).mockResolvedValue(
    ["One", "Two"].map((name, index) => ({
      id: `p${index + 1}`,
      name,
      description: null,
      owner_email: null,
      session_count: 0,
      created_at: "",
      updated_at: "",
    })),
  );
  vi.mocked(api.setSessionProject).mockImplementation(async (_id, next) => {
    projectId = next!;
    return { project_id: next };
  });
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  function View() {
    const [open, setOpen] = useState(true);
    return (
      <>
        <button onClick={() => setOpen(true)}>Reopen project</button>
        {open && <ProjectAssignDialog sessionId="s" onOpenChange={setOpen} />}
      </>
    );
  }
  render(
    <QueryClientProvider client={client}>
      <MemoryRouter>
        <ToastProvider>
          <View />
        </ToastProvider>
      </MemoryRouter>
    </QueryClientProvider>,
  );
  const input = await screen.findByLabelText("Assign to project");
  await waitFor(() => expect(input).toHaveAttribute("placeholder", "One"));
  await userEvent.click(input);
  await userEvent.click(screen.getByRole("button", { name: "Two" }));
  await userEvent.click(screen.getByRole("button", { name: "Save" }));
  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  await userEvent.click(screen.getByRole("button", { name: "Reopen project" }));
  expect(await screen.findByLabelText("Assign to project")).toHaveAttribute(
    "placeholder",
    "Two",
  );
  expect(api.getSession).not.toHaveBeenCalled();
});

it("loads and assigns a hosted-private project through the selected host without central calls", async () => {
  vi.clearAllMocks();
  const { hostControl } = await import("@/lib/hostControl");
  let membership = "p1";
  const projects = ["One", "Two"].map((name, index) => ({
    id: `p${index + 1}`,
    name,
    description: null,
    owner_email: null,
    session_count: 0,
    created_at: "",
    updated_at: "",
  }));
  const operation = vi.fn(
    async (op: string, _session: string, args: Record<string, unknown>) => {
      if (op === "projects.list")
        return {
          status: "executed",
          detail: null,
          body: { kind: "projects", projects },
        };
      if (op === "session.project.set") {
        membership = args.project_id as string;
        return {
          status: "executed",
          detail: JSON.stringify({ project_id: membership }),
          body: null,
        };
      }
      return {
        status: "executed",
        detail: null,
        body: { kind: "project_membership", project_id: membership },
      };
    },
  );
  const read = vi
    .spyOn(hostControl, "privateRead")
    .mockImplementation(async (_host, callback) =>
      callback({ operation } as never),
    );
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const close = vi.fn();
  render(
    <QueryClientProvider client={client}>
      <MemoryRouter>
        <ToastProvider>
          <ProjectAssignDialog
            sessionId="s"
            hostId="host-a"
            onOpenChange={close}
          />
        </ToastProvider>
      </MemoryRouter>
    </QueryClientProvider>,
  );
  const input = await screen.findByLabelText("Assign to project");
  await userEvent.click(input);
  await userEvent.click(await screen.findByRole("button", { name: "Two" }));
  await userEvent.click(screen.getByRole("button", { name: "Save" }));
  await waitFor(() => expect(close).toHaveBeenCalledWith(false));
  expect(operation).toHaveBeenCalledWith("projects.list", "host", {});
  expect(operation).toHaveBeenCalledWith("session.project.set", "s", {
    session: "s",
    project_id: "p2",
  });
  expect(read.mock.calls.every(([host]) => host === "host-a")).toBe(true);
  expect(api.getSession).not.toHaveBeenCalled();
  expect(api.projects).not.toHaveBeenCalled();
  expect(api.setSessionProject).not.toHaveBeenCalled();
  expect(localRequest).not.toHaveBeenCalled();
  read.mockRestore();
});
