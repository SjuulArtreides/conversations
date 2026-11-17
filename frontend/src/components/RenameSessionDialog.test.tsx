import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { RenameSessionDialog } from "./RenameSessionDialog";

vi.mock("@/lib/api", () => ({
  api: {
    getSession: vi.fn().mockResolvedValue({ project_id: "p1" }),
    projects: vi.fn().mockResolvedValue([
      { id: "p1", name: "Platform" },
      { id: "p2", name: "Conversations" },
    ]),
    setSessionProject: vi.fn().mockResolvedValue({ project_id: "p2" }),
    setTitle: vi.fn().mockResolvedValue("New name"),
  },
}));

import { api } from "@/lib/api";

beforeEach(() => {
  vi.clearAllMocks();
});

describe("RenameSessionDialog", () => {
  it("seeds the field with the current title and PUTs the trimmed new title", async () => {
    const user = userEvent.setup();
    const onOpenChange = vi.fn();
    renderWithProviders(
      <RenameSessionDialog
        id="s1"
        title="Old name"
        open
        onOpenChange={onOpenChange}
      />,
    );

    expect(screen.queryByRole("combobox")).not.toBeInTheDocument();
    expect(api.getSession).not.toHaveBeenCalled();
    const input = screen.getByLabelText("Session title");
    expect(input).toHaveValue("Old name");

    await user.clear(input);
    await user.type(input, "  New name  ");
    await user.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => {
      expect(api.setTitle).toHaveBeenCalledWith("s1", "New name");
    });
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
  });

  it("submits an empty string when cleared, to revert to the parsed title", async () => {
    const user = userEvent.setup();
    renderWithProviders(
      <RenameSessionDialog
        id="s2"
        title="Custom"
        open
        onOpenChange={() => {}}
      />,
    );

    await user.clear(screen.getByLabelText("Session title"));
    await user.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => {
      expect(api.setTitle).toHaveBeenCalledWith("s2", "");
    });
  });
});

it("uses a host rename adapter without writing the central session title", async () => {
  const rename = vi.fn().mockResolvedValue("Host title");
  const close = vi.fn();
  renderWithProviders(
    <RenameSessionDialog
      id="private-id"
      title="Old"
      open
      onOpenChange={close}
      onRename={rename}
    />,
  );
  await userEvent.clear(screen.getByLabelText("Session title"));
  await userEvent.type(screen.getByLabelText("Session title"), " Host title ");
  await userEvent.click(screen.getByRole("button", { name: "Save" }));
  await waitFor(() => expect(rename).toHaveBeenCalledWith("Host title"));
  expect(api.setTitle).not.toHaveBeenCalled();
  await waitFor(() => expect(close).toHaveBeenCalledWith(false));
});
