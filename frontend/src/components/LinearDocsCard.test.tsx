import { SessionLinearProvider } from "@/lib/sessionLinearSource";
import { hostControl } from "@/lib/hostControl";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import type { SessionLinearDoc } from "@/lib/types";
import { LinearDocsCard } from "./LinearDocsCard";

vi.mock("@/lib/api", () => ({
  api: {
    listSessionLinearDocs: vi.fn(),
    unlinkSessionLinearDoc: vi.fn(),
  },
}));

import { api } from "@/lib/api";

const mockedList = vi.mocked(api.listSessionLinearDocs);
const mockedUnlink = vi.mocked(api.unlinkSessionLinearDoc);

const DOCS: SessionLinearDoc[] = [
  {
    url: "https://linear.app/example/document/spec-1",
    workspace: "example",
    kind: "document",
    identifier: "spec-1",
  },
  {
    url: "https://linear.app/example/issue/ENG-7",
    workspace: "example",
    kind: "issue",
    identifier: "ENG-7",
  },
];

describe("LinearDocsCard", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("lists detected Linear refs with kind badge + link", async () => {
    mockedList.mockResolvedValue(DOCS);
    renderWithProviders(<LinearDocsCard sessionId="s1" />);

    const issueLink = await screen.findByRole("link", { name: "ENG-7" });
    expect(issueLink).toHaveAttribute(
      "href",
      "https://linear.app/example/issue/ENG-7",
    );
    expect(screen.getByRole("link", { name: "spec-1" })).toBeInTheDocument();
    // Kind badges render (document + issue).
    expect(screen.getAllByTestId("linear-kind-badge")).toHaveLength(2);
  });

  it("shows an empty hint when nothing is detected", async () => {
    mockedList.mockResolvedValue([]);
    renderWithProviders(<LinearDocsCard sessionId="s1" />);
    expect(
      await screen.findByText(/no linear references/i),
    ).toBeInTheDocument();
  });

  it("unlinks a ref by its canonical url", async () => {
    mockedList.mockResolvedValue(DOCS);
    mockedUnlink.mockResolvedValue(undefined);
    const user = userEvent.setup();
    renderWithProviders(<LinearDocsCard sessionId="s1" />);

    await screen.findByRole("link", { name: "spec-1" });
    const rows = screen.getAllByTestId("linear-doc-row");
    // Unlink the first row (the document).
    await user.click(rows[0].querySelector("button") as HTMLButtonElement);
    await waitFor(() =>
      expect(mockedUnlink).toHaveBeenCalledWith(
        "s1",
        "https://linear.app/example/document/spec-1",
      ),
    );
  });
});

vi.mock("@/lib/hostControl", () => ({ hostControl: { privateRead: vi.fn() } }));
it("uses the same card and Unlink action on the private host without central calls", async () => {
  vi.clearAllMocks();
  const operation = vi
    .fn()
    .mockImplementation(async (op: string) =>
      op === "linear.list"
        ? { body: { kind: "linear", docs: DOCS, next: null } }
        : { status: "executed" },
    );
  vi.mocked(hostControl.privateRead).mockImplementation(((
    _host: string,
    run: (channel: unknown) => Promise<unknown>,
  ) => run({ operation })) as typeof hostControl.privateRead);
  const user = userEvent.setup();
  renderWithProviders(
    <SessionLinearProvider hostId="laptop">
      <LinearDocsCard sessionId="private" />
    </SessionLinearProvider>,
  );
  await screen.findByRole("link", { name: "spec-1" });
  await user.click(screen.getAllByRole("button", { name: "Unlink" })[0]);
  await waitFor(() =>
    expect(operation).toHaveBeenCalledWith("linear.unlink", "private", {
      session: "private",
      url: DOCS[0].url,
    }),
  );
  expect(mockedList).not.toHaveBeenCalled();
  expect(mockedUnlink).not.toHaveBeenCalled();
});
