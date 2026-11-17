import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { ShareDialog } from "./ShareDialog";

vi.mock("@/lib/api", () => ({
  api: {
    getLiveSharing: vi.fn().mockResolvedValue([]),
    setLiveSharing: vi.fn(),
    setSharing: vi.fn(),
    searchUsers: vi.fn().mockResolvedValue([]),
  },
}));

vi.mock("@/lib/hostControl", async (original) => ({
  ...(await original<typeof import("@/lib/hostControl")>()),
  hostControl: {
    sharePublication: vi.fn(),
    unsharePublication: vi.fn(),
    publicationStatus: vi.fn(),
  },
}));

import { api } from "@/lib/api";
import {
  hostControl,
  HostControlError,
  type PublicationAudience,
  type PublicationStatus,
} from "@/lib/hostControl";
import { pairHost, unpairHost } from "@/lib/hostPairing";
import { makeTestHost } from "@/lib/testHost";

/** What the host holds; share/unshare change it, as the real host does. */
let held: PublicationStatus;
const statusOf = (
  state: PublicationStatus["state"],
  audience: PublicationAudience | null,
): PublicationStatus => ({
  state,
  rev: 3,
  audience,
  published: 4,
  pending: 0,
  error: null,
});

let HOST: string;
let managed: {
  id: string;
  title: string;
  visibility: "restricted";
  shared_emails: string[];
  publication: { state: "active"; rev: number; host_id?: string };
};

beforeEach(async () => {
  vi.clearAllMocks();
  held = statusOf("shared", {
    visibility: "restricted",
    emails: ["bob@x.test"],
  });
  vi.mocked(hostControl.publicationStatus).mockImplementation(async () => held);
  vi.mocked(hostControl.sharePublication).mockImplementation(
    async (_h, _s, audience) => {
      held = statusOf("syncing", audience);
      return "syncing";
    },
  );
  vi.mocked(hostControl.unsharePublication).mockImplementation(async () => {
    held = statusOf("deletion_pending", null);
    return "deletion_pending";
  });
  localStorage.clear();
  // This browser paired the publishing host from its descriptor.
  const host = await makeTestHost();
  await pairHost(host.descriptor, "laptop");
  HOST = host.id;
  managed = {
    id: "s1",
    title: "Fix the deploy",
    visibility: "restricted",
    shared_emails: ["bob@x.test"],
    publication: { state: "active", rev: 3, host_id: HOST },
  };
});

describe("ShareDialog for a session its host publishes", () => {
  it("changes the audience through the publishing host, never the legacy writer", async () => {
    const user = userEvent.setup();
    renderWithProviders(
      <ShareDialog session={managed} open onOpenChange={() => {}} />,
    );
    expect(
      await screen.findByText(/shared/i, {
        selector: "[data-publication-state]",
      }),
    ).toBeInTheDocument();
    expect(hostControl.publicationStatus).toHaveBeenCalledWith(HOST, "s1");

    await user.click(screen.getAllByRole("radio")[0]);
    await user.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() =>
      expect(hostControl.sharePublication).toHaveBeenCalledWith(HOST, "s1", {
        visibility: "organization",
      }),
    );
    expect(
      await screen.findByText(/syncing/i, {
        selector: "[data-publication-state]",
      }),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: /stop sharing/i }));
    await waitFor(() =>
      expect(hostControl.unsharePublication).toHaveBeenCalledWith(HOST, "s1"),
    );
    expect(
      await screen.findByText(/deletion pending/i, {
        selector: "[data-publication-state]",
      }),
    ).toBeInTheDocument();
    expect(api.setSharing).not.toHaveBeenCalled();
    expect(api.setLiveSharing).not.toHaveBeenCalled();
    expect(
      screen.queryByRole("button", { name: /manage person access/i }),
    ).not.toBeInTheDocument();
  });

  it("starts from the audience the host holds, not a default", async () => {
    const user = userEvent.setup();
    held = statusOf("shared", { visibility: "organization" });
    renderWithProviders(
      <ShareDialog session={managed} open onOpenChange={() => {}} />,
    );
    await screen.findByText(/shared/i, {
      selector: "[data-publication-state]",
    });
    // The server's copy says restricted to bob; the host, which decides, says org.
    expect(
      screen.getByRole("radio", { name: /entire organization/i }),
    ).toBeChecked();
    await user.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() =>
      expect(hostControl.sharePublication).toHaveBeenCalledWith(HOST, "s1", {
        visibility: "organization",
      }),
    );
  });

  it("shows the named readers the host holds and saves them unchanged", async () => {
    const user = userEvent.setup();
    held = statusOf("shared", {
      visibility: "restricted",
      emails: ["carol@x.test", "dave@x.test"],
    });
    renderWithProviders(
      <ShareDialog session={managed} open onOpenChange={() => {}} />,
    );
    // The unified dialog shows each reader twice: in the audience chips and
    // in the per-person "who can type" list.
    expect((await screen.findAllByText("carol@x.test")).length).toBe(2);
    expect(screen.getAllByText("dave@x.test").length).toBe(2);
    expect(
      screen.getByRole("radio", { name: /specific people/i }),
    ).toBeChecked();
    await user.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() =>
      expect(hostControl.sharePublication).toHaveBeenCalledWith(HOST, "s1", {
        visibility: "restricted",
        emails: ["carol@x.test", "dave@x.test"],
      }),
    );
  });

  it("claims nothing and changes nothing until the host has answered", async () => {
    let answer!: (s: PublicationStatus) => void;
    vi.mocked(hostControl.publicationStatus).mockReturnValue(
      new Promise((resolve) => (answer = resolve)),
    );
    renderWithProviders(
      <ShareDialog session={managed} open onOpenChange={() => {}} />,
    );
    await waitFor(() =>
      expect(hostControl.publicationStatus).toHaveBeenCalled(),
    );
    expect(screen.queryByText(/is published by its host/i)).toBeNull();
    expect(screen.queryByText(/is not published/i)).toBeNull();
    expect(
      screen.getByRole("button", { name: /^(save|publish)$/i }),
    ).toBeDisabled();
    expect(
      screen.getByRole("button", { name: /stop sharing/i }),
    ).toBeDisabled();
    await act(async () => answer(held));
    expect(
      await screen.findByRole("button", { name: /^save$/i }),
    ).toBeEnabled();
    // The unified dialog describes the same contract in its own words.
    expect(screen.getByText(/never lets anyone type/i)).toBeInTheDocument();
  });

  it("says so when the host is offline, and keeps the published copy as is", async () => {
    vi.mocked(hostControl.publicationStatus).mockRejectedValue(
      new HostControlError("The host is offline.", "offline"),
    );
    renderWithProviders(
      <ShareDialog session={managed} open onOpenChange={() => {}} />,
    );
    expect(await screen.findByText(/host is offline/i)).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /^(save|publish)$/i }),
    ).toBeDisabled();
    expect(api.setSharing).not.toHaveBeenCalled();
  });

  it("offers a reader no sharing controls at all", async () => {
    renderWithProviders(
      <ShareDialog
        session={{ ...managed, publication: { state: "active", rev: 3 } }}
        open
        onOpenChange={() => {}}
      />,
    );
    expect(
      await screen.findByText(/only its owner can change/i),
    ).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^save$/i })).toBeNull();
    expect(screen.queryByRole("button", { name: /stop sharing/i })).toBeNull();
    expect(hostControl.publicationStatus).not.toHaveBeenCalled();
  });

  it("with auth off, any browser that paired the named host gets the signed controls", async () => {
    // The auth-off marker: no owner, the host named to every reader.
    const user = userEvent.setup();
    renderWithProviders(
      <ShareDialog session={managed} open onOpenChange={() => {}} />,
    );
    await user.click(
      await screen.findByRole("button", { name: /stop sharing/i }),
    );
    await waitFor(() =>
      expect(hostControl.unsharePublication).toHaveBeenCalledWith(HOST, "s1"),
    );
    expect(api.setSharing).not.toHaveBeenCalled();
  });

  it("offers no controls for a named host this browser has not paired", async () => {
    unpairHost(HOST);
    renderWithProviders(
      <ShareDialog session={managed} open onOpenChange={() => {}} />,
    );
    expect(await screen.findByText(/has not paired/i)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^save$/i })).toBeNull();
    expect(screen.queryByRole("button", { name: /stop sharing/i })).toBeNull();
    expect(hostControl.publicationStatus).not.toHaveBeenCalled();
    expect(hostControl.unsharePublication).not.toHaveBeenCalled();
    expect(api.setSharing).not.toHaveBeenCalled();
  });

  it("keeps the legacy writer for an unmanaged session", async () => {
    const user = userEvent.setup();
    vi.mocked(api.setSharing).mockResolvedValue({
      visibility: "organization",
      emails: [],
    });
    renderWithProviders(
      <ShareDialog
        session={{ ...managed, publication: null }}
        open
        onOpenChange={() => {}}
      />,
    );
    await user.click(screen.getAllByRole("radio")[0]);
    await user.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() => expect(api.setSharing).toHaveBeenCalled());
    expect(hostControl.sharePublication).not.toHaveBeenCalled();
  });
});
