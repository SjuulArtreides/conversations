import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import type { RepoLink } from "@/lib/types";
import { RepoDialog } from "./RepoDialog";

vi.mock("@/lib/api", () => ({
  api: {
    setRepos: vi.fn().mockResolvedValue([]),
  },
}));

import { api } from "@/lib/api";

const repo: RepoLink = {
  repo: "my-service",
  remote_url: "git@github.com:org/my-service.git",
  branch: "feat/x",
  pr_url: null,
  paths: [{ user_email: "alice@x.com", local_path: "/Users/alice/my-service" }],
};

const session = {
  id: "s1",
  title: "Fix the deploy",
  repos: [repo],
};

beforeEach(() => {
  vi.clearAllMocks();
});

describe("RepoDialog", () => {
  it("seeds rows from the session repos and PUTs the canonical edit (dropping paths)", async () => {
    const user = userEvent.setup();
    renderWithProviders(
      <RepoDialog session={session} open onOpenChange={() => {}} />,
    );

    // Existing repo is pre-filled.
    expect(screen.getByDisplayValue("my-service")).toBeInTheDocument();
    expect(screen.getByDisplayValue("feat/x")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => {
      expect(api.setRepos).toHaveBeenCalledWith("s1", [
        {
          repo: "my-service",
          branch: "feat/x",
          remote_url: "git@github.com:org/my-service.git",
          pr_url: null,
        },
      ]);
    });
  });

  it("drops rows missing a repo name or branch before saving", async () => {
    const user = userEvent.setup();
    renderWithProviders(
      <RepoDialog
        session={{ id: "s2", title: "t", repos: [] }}
        open
        onOpenChange={() => {}}
      />,
    );

    // Add an empty row, fill only the repo (no branch) → should be dropped.
    await user.click(screen.getByRole("button", { name: /add repository/i }));
    const repoInput = screen.getByPlaceholderText("my-service");
    await user.type(repoInput, "lonely-repo");

    await user.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => {
      expect(api.setRepos).toHaveBeenCalledWith("s2", []);
    });
  });
});
