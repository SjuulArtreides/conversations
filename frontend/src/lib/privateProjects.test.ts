import { afterEach, expect, it, vi } from "vitest";
import { privateProjects } from "./privateProjects";
import { hostControl } from "./hostControl";
import { checkBody } from "./privateChannel";
vi.mock("./hostControl", () => ({ hostControl: { privateRead: vi.fn() } }));
afterEach(() => vi.resetAllMocks());
it("fails closed on refused host project mutation and never falls back", async () => {
  const operation = vi
    .fn()
    .mockResolvedValue({
      status: "refused",
      detail: "owner grant required",
      body: null,
    });
  vi.mocked(hostControl.privateRead).mockImplementation(async (_host, read) =>
    read({ operation } as never),
  );
  await expect(privateProjects.assign("host-a", "s", "p")).rejects.toThrow(
    "owner grant required",
  );
  expect(operation).toHaveBeenCalledTimes(1);
});
it("validates project list and membership wire bodies before rendering", () => {
  const project = {
    id: "p",
    name: "One",
    description: null,
    owner_email: null,
    session_count: 2,
    created_at: "now",
    updated_at: "now",
  };
  expect(checkBody({ kind: "projects", projects: [project] })).toEqual({
    kind: "projects",
    projects: [project],
  });
  expect(checkBody({ kind: "project_membership", project_id: null })).toEqual({
    kind: "project_membership",
    project_id: null,
  });
  expect(() =>
    checkBody({
      kind: "projects",
      projects: [{ ...project, session_count: -1 }],
    }),
  ).toThrow();
  expect(() =>
    checkBody({ kind: "project_membership", project_id: 42 }),
  ).toThrow();
});
