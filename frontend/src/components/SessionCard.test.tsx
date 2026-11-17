import { describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { render } from "@testing-library/react";
import { SessionCard } from "./SessionCard";
import type { Summary } from "@/lib/types";

function summary(overrides: Partial<Summary> = {}): Summary {
  return {
    id: "sess-1",
    title: "Fix the flaky deploy",
    project_path: "/Users/dev/proj",
    git_branch: "main",
    message_count: 42,
    started_at: "2026-06-05T10:00:00.000Z",
    ended_at: "2026-06-05T11:00:00.000Z",
    uploaded_by: "alice@x.com",
    uploaded_at: "2026-06-05T12:00:00.000Z",
    source_host: "host",
    owner_email: "alice@x.com",
    visibility: "organization",
    repos: [],
    ...overrides,
  };
}

function renderCard(s: Summary, opts: { live?: boolean } = {}) {
  return render(
    <MemoryRouter>
      <SessionCard session={s} live={opts.live} />
    </MemoryRouter>,
  );
}

describe("SessionCard", () => {
  it("shows the title, message count, uploader, and links to the detail page", () => {
    renderCard(summary());
    expect(screen.getByText("Fix the flaky deploy")).toBeInTheDocument();
    expect(screen.getByText(/42 messages/)).toBeInTheDocument();
    expect(screen.getByText(/uploaded by alice@x.com/)).toBeInTheDocument();
    expect(screen.getByRole("link")).toHaveAttribute(
      "href",
      "/sessions/sess-1",
    );
  });

  it("links a live card straight to the fullscreen terminal shell", () => {
    renderCard(summary(), { live: true });
    expect(screen.getByRole("link")).toHaveAttribute(
      "href",
      "/sessions/sess-1?fullscreen=1",
    );
  });

  it("keeps non-live cards on the normal history route", () => {
    renderCard(summary(), { live: false });
    expect(screen.getByRole("link")).toHaveAttribute(
      "href",
      "/sessions/sess-1",
    );
  });

  it("renders the Organization badge for org-visible sessions", () => {
    renderCard(summary({ visibility: "organization" }));
    expect(screen.getByText(/Organization/)).toBeInTheDocument();
    expect(screen.queryByText(/Restricted/)).not.toBeInTheDocument();
  });

  it("renders the Restricted badge for restricted sessions", () => {
    renderCard(summary({ visibility: "restricted" }));
    expect(screen.getByText(/Restricted/)).toBeInTheDocument();
    expect(screen.queryByText(/Organization/)).not.toBeInTheDocument();
  });

  it("renders repo @ branch chips when the session has linked repos", () => {
    renderCard(
      summary({
        repos: [
          {
            repo: "my-service",
            remote_url: "git@github.com:org/my-service.git",
            branch: "feat/x",
            pr_url: null,
            paths: [],
          },
        ],
      }),
    );
    expect(screen.getByText(/my-service ⎇ feat\/x/)).toBeInTheDocument();
  });

  it("uses non-container repo chips instead of the transcript branch when repos are linked", () => {
    renderCard(
      summary({
        project_path: "/Users/dev/example",
        git_branch: "chore/top-level",
        repos: [
          {
            repo: "example",
            remote_url: "git@github.com:org/example-workspace.git",
            branch: "chore/top-level",
            pr_url: null,
            paths: [],
            is_container: true,
          },
          {
            repo: "example-platform",
            remote_url: "git@github.com:org/example-platform.git",
            branch: "fix/platform",
            pr_url: null,
            paths: [],
          },
        ],
      }),
    );

    expect(
      screen.getByText(/example-platform ⎇ fix\/platform/),
    ).toBeInTheDocument();
    expect(
      screen.queryByText(/example ⎇ chore\/top-level/),
    ).not.toBeInTheDocument();
    expect(screen.queryByText(/⎇ chore\/top-level/)).not.toBeInTheDocument();
  });

  it("falls back to the transcript branch when only container repos are linked", () => {
    renderCard(
      summary({
        git_branch: "chore/top-level",
        repos: [
          {
            repo: "example",
            remote_url: "git@github.com:org/example-workspace.git",
            branch: "chore/top-level",
            pr_url: null,
            paths: [],
            is_container: true,
          },
        ],
      }),
    );

    expect(screen.getByText(/⎇ chore\/top-level/)).toBeInTheDocument();
  });

  it("suppresses the transcript branch when the launch dir is a container root", () => {
    // No discovered repos, but the session was launched at a viewer container
    // path (`project_is_container`): the transcript branch is the container's
    // branch, so it must not render as if it were the work repo.
    renderCard(
      summary({
        project_path: "/Users/dev/example",
        git_branch: "chore/top-level",
        project_is_container: true,
        repos: [],
      }),
    );

    expect(screen.queryByText(/⎇ chore\/top-level/)).not.toBeInTheDocument();
    // The project short-name still shows (it's just the launch dir label).
    expect(screen.getByText("example")).toBeInTheDocument();
  });

  it("badges non-Claude agents but not Claude (or unspecified) sessions", () => {
    const { unmount } = renderCard(summary({ agent: "codex" }));
    expect(screen.getByText(/Codex/)).toBeInTheDocument();
    unmount();

    const { unmount: unmount2 } = renderCard(summary({ agent: "opencode" }));
    expect(screen.getByText(/OpenCode/)).toBeInTheDocument();
    unmount2();

    const { unmount: unmount3 } = renderCard(summary({ agent: "claude" }));
    expect(screen.queryByText(/Codex/)).not.toBeInTheDocument();
    expect(screen.queryByText(/OpenCode/)).not.toBeInTheDocument();
    unmount3();

    // Legacy summaries arrive without an agent field — still no chip.
    renderCard(summary());
    expect(screen.queryByText(/Codex/)).not.toBeInTheDocument();
    expect(screen.queryByText(/OpenCode/)).not.toBeInTheDocument();
  });
});
