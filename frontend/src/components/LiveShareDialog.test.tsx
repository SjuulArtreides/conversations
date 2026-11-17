import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import type { LiveShare } from "@/lib/types";
import { LiveShareDialog } from "./LiveShareDialog";

// Mock the API module the dialog + autocomplete depend on.
vi.mock("@/lib/api", () => ({
  api: {
    getLiveSharing: vi.fn(),
    setLiveSharing: vi.fn(),
    searchUsers: vi.fn().mockResolvedValue([]),
  },
}));

import { api } from "@/lib/api";

const mockedGet = vi.mocked(api.getLiveSharing);
const mockedSet = vi.mocked(api.setLiveSharing);

beforeEach(() => {
  vi.clearAllMocks();
  mockedSet.mockImplementation((_id, shares: LiveShare[]) =>
    Promise.resolve(shares),
  );
});

function renderDialog() {
  renderWithProviders(
    <LiveShareDialog
      liveId="L1"
      title="Live one"
      open
      onOpenChange={() => {}}
    />,
  );
}

/** The person's row: its transcript radio group and communication checkbox. */
async function row(email: string) {
  const transcript = await screen.findByRole("radiogroup", {
    name: `Transcript access for ${email}`,
  });
  const item = transcript.closest("li") as HTMLElement;
  return {
    transcript,
    radio: (name: RegExp) => within(transcript).getByRole("radio", { name }),
    communicate: within(item).getByRole("checkbox", { name: /communicate/i }),
  };
}

describe("LiveShareDialog", () => {
  it("shows transcript access and communication independently", async () => {
    mockedGet.mockResolvedValue([
      {
        email: "reader@x.com",
        can_read: true,
        can_write: false,
        can_communicate: false,
      },
      {
        email: "talker@x.com",
        can_read: false,
        can_write: false,
        can_communicate: true,
      },
      {
        email: "writer@x.com",
        can_read: true,
        can_write: true,
        can_communicate: true,
      },
    ]);
    renderDialog();

    const reader = await row("reader@x.com");
    expect(reader.radio(/^read$/i)).toBeChecked();
    expect(reader.communicate).not.toBeChecked();

    const talker = await row("talker@x.com");
    expect(talker.radio(/no access/i)).toBeChecked();
    expect(talker.communicate).toBeChecked();

    const writer = await row("writer@x.com");
    expect(writer.radio(/read \+ write/i)).toBeChecked();
    expect(writer.communicate).toBeChecked();
  });

  it("treats a legacy read-write share without the field as communicating", async () => {
    mockedGet.mockResolvedValue([
      { email: "old-writer@x.com", can_write: true },
      { email: "old-talker@x.com", can_write: false, communication_only: true },
    ]);
    renderDialog();
    const writer = await row("old-writer@x.com");
    expect(writer.radio(/read \+ write/i)).toBeChecked();
    expect(writer.communicate).toBeChecked();
    const talker = await row("old-talker@x.com");
    expect(talker.radio(/no access/i)).toBeChecked();
    expect(talker.communicate).toBeChecked();
  });

  it("adds communication to read access without changing transcript access", async () => {
    mockedGet.mockResolvedValue([]);
    const user = userEvent.setup();
    renderDialog();

    const input = await screen.findByLabelText(/add people by email/i);
    await user.type(input, "new@x.com");
    await user.click(
      await screen.findByRole("button", { name: /add new@x\.com/i }),
    );
    // A new person defaults to read without communication.
    const person = await row("new@x.com");
    expect(person.radio(/^read$/i)).toBeChecked();
    expect(person.communicate).not.toBeChecked();

    await user.click(person.communicate);
    await user.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() =>
      expect(mockedSet).toHaveBeenCalledWith("L1", [
        {
          email: "new@x.com",
          can_read: true,
          can_write: false,
          can_communicate: true,
          communication_only: false,
        },
      ]),
    );
  });

  it("saves communication-only and read-write without communication", async () => {
    mockedGet.mockResolvedValue([
      {
        email: "agent@x.com",
        can_read: true,
        can_write: true,
        can_communicate: true,
      },
    ]);
    const user = userEvent.setup();
    renderDialog();

    const person = await row("agent@x.com");
    await user.click(person.radio(/no access/i));
    await user.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() =>
      expect(mockedSet).toHaveBeenLastCalledWith("L1", [
        {
          email: "agent@x.com",
          can_read: false,
          can_write: false,
          can_communicate: true,
          communication_only: true,
        },
      ]),
    );

    await user.click(person.radio(/read \+ write/i));
    await user.click(person.communicate);
    await user.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() =>
      expect(mockedSet).toHaveBeenLastCalledWith("L1", [
        {
          email: "agent@x.com",
          can_read: true,
          can_write: true,
          can_communicate: false,
          communication_only: false,
        },
      ]),
    );
  });

  it("blocks saving a person who would receive no access", async () => {
    mockedGet.mockResolvedValue([
      {
        email: "talker@x.com",
        can_read: false,
        can_write: false,
        can_communicate: true,
      },
    ]);
    const user = userEvent.setup();
    renderDialog();

    const person = await row("talker@x.com");
    await user.click(person.communicate);
    expect(await screen.findByRole("alert")).toHaveTextContent(
      /talker@x\.com.*no access/i,
    );
    expect(screen.getByRole("button", { name: /^save$/i })).toBeDisabled();

    await user.click(person.radio(/^read$/i));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() =>
      expect(mockedSet).toHaveBeenCalledWith("L1", [
        {
          email: "talker@x.com",
          can_read: true,
          can_write: false,
          can_communicate: false,
          communication_only: false,
        },
      ]),
    );
  });

  it("downgrading write keeps the communication choice", async () => {
    mockedGet.mockResolvedValue([
      {
        email: "writer@x.com",
        can_read: true,
        can_write: true,
        can_communicate: true,
      },
    ]);
    const user = userEvent.setup();
    renderDialog();

    const person = await row("writer@x.com");
    expect(
      screen.getByText(/accepts only when it also holds locally/i),
    ).toBeInTheDocument();
    await user.click(person.radio(/^read$/i));
    expect(
      screen.queryByText(/accepts only when it also holds locally/i),
    ).not.toBeInTheDocument();
    expect(person.communicate).toBeChecked();
    await user.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() =>
      expect(mockedSet).toHaveBeenCalledWith("L1", [
        {
          email: "writer@x.com",
          can_read: true,
          can_write: false,
          can_communicate: true,
          communication_only: false,
        },
      ]),
    );
  });
});
