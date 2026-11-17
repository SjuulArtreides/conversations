import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { ShareDialog } from "./ShareDialog";

// Mock the API module the dialog + autocomplete depend on.
vi.mock("@/lib/api", () => ({
  api: {
    getLiveSharing: vi.fn().mockResolvedValue([]),
    setLiveSharing: vi
      .fn()
      .mockImplementation((_id, shares) => Promise.resolve(shares)),
    setSharing: vi.fn().mockResolvedValue({
      visibility: "restricted",
      emails: ["new@x.com"],
    }),
    searchUsers: vi.fn().mockResolvedValue([]),
  },
}));

import { api } from "@/lib/api";

const session = {
  id: "s1",
  title: "Fix the deploy",
  visibility: "organization" as const,
  shared_emails: [] as string[],
};

beforeEach(() => {
  vi.clearAllMocks();
});

describe("ShareDialog", () => {
  it("hides the email input until 'Specific people' is chosen, then sends the right payload", async () => {
    const user = userEvent.setup();
    renderWithProviders(
      <ShareDialog session={session} open onOpenChange={() => {}} />,
    );

    // Organization is selected initially → no email input.
    expect(
      screen.queryByLabelText(/add people by email/i),
    ).not.toBeInTheDocument();

    // Switch to "Specific people" (second radio).
    const radios = screen.getAllByRole("radio");
    expect(radios).toHaveLength(2);
    await user.click(radios[1]);

    const input = await screen.findByLabelText(/add people by email/i);
    expect(input).toBeInTheDocument();

    // Type a brand-new email and add it via the "Add …" affordance.
    await user.type(input, "new@x.com");
    const addBtn = await screen.findByRole("button", {
      name: /add new@x\.com/i,
    });
    await user.click(addBtn);

    // Chip is shown (removable).
    expect(
      screen.getByRole("button", { name: /remove new@x\.com/i }),
    ).toBeInTheDocument();

    // Save → PUT payload is { visibility: "restricted", emails: ["new@x.com"] }.
    await user.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() => {
      expect(api.setSharing).toHaveBeenCalledWith("s1", {
        visibility: "restricted",
        emails: ["new@x.com"],
      });
    });
  });

  it("opens person communication sharing for a stored session", async () => {
    const user = userEvent.setup();
    renderWithProviders(
      <ShareDialog session={session} open onOpenChange={() => {}} />,
    );
    await user.click(
      screen.getByRole("button", {
        name: /manage person access and communication/i,
      }),
    );
    await waitFor(() => expect(api.getLiveSharing).toHaveBeenCalledWith("s1"));
    const input = await screen.findByLabelText(/add people by email/i);
    await user.type(input, "agent@x.com");
    await user.click(
      await screen.findByRole("button", { name: /add agent@x.com/i }),
    );
    await user.click(screen.getByRole("radio", { name: /no access/i }));
    await user.click(screen.getByRole("checkbox", { name: /communicate/i }));
    await user.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() =>
      expect(api.setLiveSharing).toHaveBeenCalledWith("s1", [
        {
          email: "agent@x.com",
          can_read: false,
          can_write: false,
          can_communicate: true,
          communication_only: true,
        },
      ]),
    );
    expect(api.setSharing).not.toHaveBeenCalled();
  });

  it("queries the user directory as you type in the autocomplete", async () => {
    const user = userEvent.setup();
    renderWithProviders(
      <ShareDialog
        session={{ ...session, visibility: "restricted" }}
        open
        onOpenChange={() => {}}
      />,
    );

    const input = await screen.findByLabelText(/add people by email/i);
    await user.type(input, "ali");

    // Debounced (300ms) call to the directory.
    await waitFor(() => expect(api.searchUsers).toHaveBeenCalledWith("ali"), {
      timeout: 1500,
    });
  });
});
