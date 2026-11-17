import { hostControl } from "./hostControl";
import { HOST_SCOPE, type OperationName } from "./hostAuth";
import { checkBody } from "./privateChannel";

async function operation(
  hostId: string,
  op: OperationName,
  session: string,
  args: Record<string, unknown> = {},
) {
  const result = await hostControl.privateRead(
    hostId,
    (channel) => channel.operation(op, session, args),
    { allowUnlock: op === "projects.create" || op === "session.project.set" },
  );
  if (result.status !== "executed" && result.status !== "replayed")
    throw new Error(result.detail ?? "The host could not update projects.");
  return result;
}
export const privateProjects = {
  async list(hostId: string) {
    const result = await operation(hostId, "projects.list", HOST_SCOPE);
    if (result.body?.kind !== "projects")
      throw new Error("The host returned unreadable projects.");
    return result.body.projects;
  },
  async membership(hostId: string, session: string) {
    const result = await operation(hostId, "session.project.get", session, {
      session,
    });
    if (result.body?.kind !== "project_membership")
      throw new Error("The host returned unreadable project membership.");
    return { project_id: result.body.project_id };
  },
  async create(hostId: string, name: string) {
    const result = await operation(hostId, "projects.create", HOST_SCOPE, {
      name,
    });
    const body = checkBody({
      kind: "projects",
      projects: [JSON.parse(result.detail ?? "null")],
    });
    if (body?.kind !== "projects")
      throw new Error("The host returned an unreadable project.");
    return body.projects[0];
  },
  async assign(hostId: string, session: string, project_id: string | null) {
    const result = await operation(hostId, "session.project.set", session, {
      session,
      project_id,
    });
    const body = checkBody({
      kind: "project_membership",
      ...JSON.parse(result.detail ?? "null"),
    });
    if (body?.kind !== "project_membership")
      throw new Error("The host returned unreadable project membership.");
    return { project_id: body.project_id };
  },
};
