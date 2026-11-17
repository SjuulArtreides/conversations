import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { HostSharing } from "./HostSharing";

const control = vi.hoisted(() => ({
  addGrant: vi.fn(),
  removeGrant: vi.fn(),
  listAccess: vi.fn(),
}));
vi.mock("@/lib/hostControl", async () => ({
  ...(await vi.importActual("@/lib/hostControl")),
  hostControl: control,
}));
import { HostControlError } from "@/lib/hostControl";

const SESSION = "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11";
const CREDENTIAL = "C".repeat(43);

beforeEach(() => {
  vi.clearAllMocks();
  control.listAccess.mockRejectedValue(
    new HostControlError(
      "Owner grant required to list approved credentials.",
      "refused",
    ),
  );
});

describe("HostSharing", () => {
  it("stays disabled until both ids are well formed, then asks the host for read-only access by default", async () => {
    control.addGrant.mockResolvedValue(undefined);
    const user = userEvent.setup();
    renderWithProviders(<HostSharing />);
    const button = screen.getByRole("button", {
      name: "Allow selected access",
    });
    expect(button).toBeDisabled();
    await user.type(screen.getByLabelText("Session id"), "not-a-uuid");
    await user.type(await screen.findByLabelText("Credential id"), CREDENTIAL);
    expect(button).toBeDisabled();
    await user.clear(screen.getByLabelText("Session id"));
    await user.type(screen.getByLabelText("Session id"), SESSION);
    expect(button).toBeEnabled();
    await user.click(button);
    await waitFor(() =>
      expect(control.addGrant).toHaveBeenCalledWith(
        SESSION,
        CREDENTIAL,
        ["history.read"],
        undefined, // the active host: this form names no other
      ),
    );
    expect(
      await screen.findByText(/can now read this session/i),
    ).toBeInTheDocument();
  });

  it("reports the host's refusal instead of claiming the session was shared", async () => {
    control.addGrant.mockRejectedValue(
      new Error("The host refused this: no current grant."),
    );
    const user = userEvent.setup();
    renderWithProviders(<HostSharing />);
    await user.type(screen.getByLabelText("Session id"), SESSION);
    await user.type(await screen.findByLabelText("Credential id"), CREDENTIAL);
    await user.click(
      screen.getByRole("button", { name: "Allow selected access" }),
    );
    expect(await screen.findByText(/no current grant/)).toBeInTheDocument();
    expect(screen.queryByText(/can now write/i)).not.toBeInTheDocument();
  });
});

describe("HostSharing removal", () => {
  it("takes only the selected capabilities back from the credential", async () => {
    control.removeGrant.mockResolvedValue(undefined);
    const user = userEvent.setup();
    renderWithProviders(<HostSharing session={SESSION} />);
    await user.type(await screen.findByLabelText("Credential id"), CREDENTIAL);
    await user.click(screen.getByRole("button", { name: "Remove access" }));
    await waitFor(() =>
      expect(control.removeGrant).toHaveBeenCalledWith(
        SESSION,
        CREDENTIAL,
        ["history.read"],
        undefined,
      ),
    );
    expect(control.addGrant).not.toHaveBeenCalled();
    expect(
      await screen.findByText(/Removed the selected session grants/i),
    ).toBeInTheDocument();
  });

  it("reports a refused removal instead of claiming access was removed", async () => {
    control.removeGrant.mockRejectedValue(
      new Error("The host refused this: not authorized."),
    );
    const user = userEvent.setup();
    renderWithProviders(<HostSharing session={SESSION} />);
    await user.type(await screen.findByLabelText("Credential id"), CREDENTIAL);
    await user.click(screen.getByRole("button", { name: "Remove access" }));
    expect(await screen.findByText(/not authorized/)).toBeInTheDocument();
    expect(
      screen.queryByText(/Removed the selected session grants/i),
    ).not.toBeInTheDocument();
  });
});

it("requires explicit write opt-in and refuses an empty capability selection", async () => {
  control.addGrant.mockResolvedValue(undefined);
  renderWithProviders(<HostSharing session={SESSION} hostId="host-a" />);
  await userEvent.type(
    await screen.findByLabelText("Credential id"),
    CREDENTIAL,
  );
  await userEvent.click(
    screen.getByRole("checkbox", { name: "Read this session’s history" }),
  );
  expect(
    screen.getByRole("button", { name: "Allow selected access" }),
  ).toBeDisabled();
  await userEvent.click(
    screen.getByRole("checkbox", { name: "Write to this session" }),
  );
  await userEvent.click(
    screen.getByRole("button", { name: "Allow selected access" }),
  );
  await waitFor(() =>
    expect(control.addGrant).toHaveBeenCalledWith(
      SESSION,
      CREDENTIAL,
      ["session.write"],
      "host-a",
    ),
  );
});

it("selects only enrolled active keys, shows the full fingerprint, and defaults to read-only", async () => {
  const revoked = "D".repeat(43);
  control.listAccess.mockResolvedValue([
    {
      kid: CREDENTIAL,
      label: "My browser",
      alg: "es256",
      kind: "owner",
      revoked: false,
      grants: [],
    },
    {
      kid: revoked,
      label: "Old browser",
      alg: "es256",
      kind: "owner",
      revoked: true,
      grants: [],
    },
  ]);
  control.addGrant.mockResolvedValue(undefined);
  renderWithProviders(<HostSharing session={SESSION} hostId="host-a" />);
  const picker = await screen.findByRole("combobox", { name: "Approved key" });
  expect(
    screen.getByRole("option", { name: "Old browser (revoked)" }),
  ).toBeDisabled();
  expect(screen.queryByLabelText("Credential id")).toBeNull();
  await userEvent.selectOptions(picker, CREDENTIAL);
  expect(screen.getByLabelText("Credential fingerprint").textContent).toBe(
    CREDENTIAL,
  );
  expect(
    screen.getByRole("checkbox", { name: "Read this session’s history" }),
  ).toBeChecked();
  expect(
    screen.getByRole("checkbox", { name: "Write to this session" }),
  ).not.toBeChecked();
  await userEvent.click(
    screen.getByRole("button", { name: "Allow selected access" }),
  );
  await waitFor(() =>
    expect(control.addGrant).toHaveBeenCalledWith(
      SESSION,
      CREDENTIAL,
      ["history.read"],
      "host-a",
    ),
  );
});
it("does not offer manual credentials after a transport failure", async () => {
  control.listAccess.mockRejectedValue(
    new HostControlError("Connection dropped", "transport"),
  );
  renderWithProviders(<HostSharing session={SESSION} hostId="host-a" />);
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Connection dropped",
  );
  expect(screen.queryByLabelText("Credential id")).toBeNull();
  expect(
    screen.getByRole("button", { name: "Allow selected access" }),
  ).toBeDisabled();
});
