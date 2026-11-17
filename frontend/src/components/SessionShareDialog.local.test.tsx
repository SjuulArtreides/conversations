import { beforeEach, expect, it, vi } from "vitest";
import { act, screen } from "@testing-library/react";
import { renderWithProviders } from "@/test/utils";
import { SessionShareDialog } from "./SessionShareDialog";
import { LOCAL_HOST_ID, localRequest } from "@/lib/localHost";
import { hostControl } from "@/lib/hostControl";
import { makeTestHost } from "@/lib/testHost";
import { pairCode } from "@/lib/pairCode";

vi.mock("@/lib/localHost", () => ({
  LOCAL_HOST_ID: "local",
  localRequest: vi.fn(),
  useLocalHost: () => ({ local: true, authenticated: true, error: null }),
}));
vi.mock("@/lib/hostControl", async (original) => ({
  ...(await original<typeof import("@/lib/hostControl")>()),
  hostControl: {
    publicationStatus: vi.fn(),
    passkeyPending: vi.fn(),
    listAccess: vi.fn(),
  },
}));
vi.mock("@/lib/api", () => ({
  api: { searchUsers: vi.fn().mockResolvedValue([]) },
}));

beforeEach(() => {
  vi.clearAllMocks();
  localStorage.clear();
});

it("derives a local owner's pair code from the authenticated host descriptor", async () => {
  const owner = await makeTestHost();
  const colleague = await makeTestHost();
  let resolveStatus!: (status: { descriptor: string }) => void;
  vi.mocked(localRequest).mockReturnValue(
    new Promise((resolve) => {
      resolveStatus = resolve;
    }),
  );
  vi.mocked(hostControl.publicationStatus).mockResolvedValue({
    state: "private",
    rev: 0,
    audience: null,
    published: 0,
    pending: 0,
    error: null,
  });
  vi.mocked(hostControl.listAccess).mockResolvedValue([]);
  vi.mocked(hostControl.passkeyPending).mockResolvedValue([
    {
      cred_id: "C".repeat(43),
      label: "Colleague",
      kind: "agent",
      grants: [],
      vouch_host_id: colleague.id,
      vouch_host_key: colleague.key,
    },
  ]);
  renderWithProviders(
    <SessionShareDialog
      hostId={LOCAL_HOST_ID}
      sessionId="session"
      title="Work"
      open
      onOpenChange={() => {}}
    />,
  );
  expect(await screen.findByText("Loading pair code…")).toBeInTheDocument();
  expect(screen.queryByText(/No pair code:/)).not.toBeInTheDocument();
  expect(
    screen.getByRole("checkbox", { name: "Confirm this key: Colleague" }),
  ).toBeDisabled();
  expect(screen.getByRole("button", { name: "Trust this key" })).toBeDisabled();
  await act(async () => resolveStatus({ descriptor: owner.descriptor }));
  const expected = await pairCode(owner.key, colleague.key);
  expect(
    await screen.findByTestId(`pair-code-${"C".repeat(43)}`),
  ).toHaveTextContent(expected);
  expect(localRequest).toHaveBeenCalledWith("/api/status");
  expect(screen.queryByText(/No pair code:/)).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Trust this key" })).toBeDisabled();
});
