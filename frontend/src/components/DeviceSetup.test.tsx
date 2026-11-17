import { beforeEach, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { DeviceSetup } from "./DeviceSetup";
import { localRequest } from "@/lib/localHost";
vi.mock("@/lib/localHost", () => ({ localRequest: vi.fn() }));
const id = "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789abcdEFG";
beforeEach(() => {
  vi.clearAllMocks();
});
it("requires the laptop owner to match the phone's code before approving its exact registration", async () => {
  vi.mocked(localRequest).mockImplementation(async (path) => {
    if (path === "/api/devices/pending")
      return {
        devices: [
          {
            cred_id: id,
            label: "Conversations phone",
            grants: ["history.read", "session.write"],
          },
        ],
      } as never;
    if (path === "/api/devices/start")
      return { link: "https://example.test/host-link#desc=synthetic" } as never;
    return { ok: true } as never;
  });
  const user = userEvent.setup();
  renderWithProviders(<DeviceSetup />);
  await user.click(screen.getByRole("button", { name: "Add a device" }));
  await user.click(screen.getByRole("button", { name: "Create setup code" }));
  expect(await screen.findByText("Device waiting for approval")).toBeVisible();
  const approve = screen.getByRole("button", { name: "Approve device" });
  expect(approve).toBeDisabled();
  await user.type(
    screen.getByLabelText("Enter the matching code"),
    id.slice(-12),
  );
  expect(approve).toBeEnabled();
  await user.click(approve);
  await waitFor(() =>
    expect(localRequest).toHaveBeenCalledWith(
      "/api/devices/approve",
      expect.objectContaining({
        method: "POST",
        body: JSON.stringify({ cred_id: id }),
      }),
    ),
  );
  expect(await screen.findByText(/Device approved/)).toBeVisible();
});

it("regenerates an expired setup code and reports clipboard failure", async () => {
  let starts = 0;
  vi.mocked(localRequest).mockImplementation(async (path) => {
    if (path === "/api/devices/pending") return { devices: [] } as never;
    if (path === "/api/devices/start")
      return {
        link: `https://example.test/host-link#desc=fixture${++starts}`,
      } as never;
    return { ok: true } as never;
  });
  const user = userEvent.setup();
  renderWithProviders(<DeviceSetup />);
  await user.click(screen.getByRole("button", { name: "Add a device" }));
  await user.click(screen.getByRole("button", { name: "Create setup code" }));
  expect(await screen.findByTitle("Phone setup QR code")).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Close device setup" }));
  await user.click(screen.getByRole("button", { name: "Add a device" }));
  await user.click(screen.getByRole("button", { name: "Create a new code" }));
  expect(starts).toBe(2);
  vi.spyOn(navigator.clipboard, "writeText").mockRejectedValue(
    new Error("denied"),
  );
  await user.click(screen.getByRole("button", { name: "Copy setup link" }));
  expect(await screen.findByText(/Could not copy the link/)).toBeVisible();
});
