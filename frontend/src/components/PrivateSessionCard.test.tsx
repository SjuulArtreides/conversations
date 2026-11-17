import { describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import { renderWithProviders } from "@/test/utils";
import { PrivateSessionCard, privateSessionTitle } from "./PrivateSessionCard";
import type { SessionEntry } from "@/lib/privateChannel";
const entry: SessionEntry = {
  id: "11111111-2222-3333-4444-555555555555",
  agent: "codex",
  cwd: "/Users/me/example",
  title: null,
  running: true,
  messages: 3,
  last_activity: null,
};
describe("private cards", () => {
  it.each([
    null,
    entry.id,
    "# AGENTS.md instructions for /Users/me/example",
    "<environment_context>root</environment_context>",
    "You are Codex, an agent based on GPT-6.",
  ])("uses a readable fallback for %s", (title) => {
    expect(privateSessionTitle({ ...entry, title })).toBe("Codex · example");
  });
  it("normalizes and bounds titles without splitting Unicode", () => {
    expect(
      privateSessionTitle({ ...entry, title: "  Fix\n the build  " }),
    ).toBe("Fix the build");
    const title = privateSessionTitle({ ...entry, title: "😀".repeat(200) });
    expect(Array.from(title)).toHaveLength(120);
    expect(title.endsWith("😀…")).toBe(true);
  });
  it("shows private agent, live, project and message information", () => {
    renderWithProviders(<PrivateSessionCard session={entry} />);
    // Running card opens the live terminal directly; carrying `?fullscreen=1`
    // lets the detail route paint the terminal shell on the first render.
    expect(screen.getByRole("link")).toHaveAttribute(
      "href",
      `/private/${entry.id}?fullscreen=1`,
    );
    for (const label of [
      "Codex · example",
      "Private",
      "Live",
      "example",
      "3 messages",
    ])
      expect(screen.getByText(label)).toBeInTheDocument();
    expect(screen.queryByText(/uploaded by|Users/)).toBeNull();
  });

  it("keeps stopped cards on the normal history route", () => {
    renderWithProviders(
      <PrivateSessionCard session={{ ...entry, running: false }} />,
    );
    expect(screen.getByRole("link")).toHaveAttribute(
      "href",
      `/private/${entry.id}`,
    );
  });
});
