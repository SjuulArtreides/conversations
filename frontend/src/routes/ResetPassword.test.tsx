import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { ResetPassword } from "./ResetPassword";

vi.mock("@/lib/api", async () => {
  const actual = await vi.importActual<typeof import("@/lib/api")>("@/lib/api");
  return {
    ...actual,
    api: {
      ...actual.api,
      resetPassword: vi.fn(),
    },
  };
});

import { api } from "@/lib/api";

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(api.resetPassword).mockResolvedValue({ ok: true });
});

describe("ResetPassword", () => {
  it("changes the password with the token from the link", async () => {
    const user = userEvent.setup();
    renderWithProviders(<ResetPassword />, {
      route: "/reset-password?token=one-time-token",
    });

    await user.type(screen.getByLabelText("New password"), "new-password");
    await user.type(
      screen.getByLabelText("Confirm new password"),
      "new-password",
    );
    await user.click(screen.getByRole("button", { name: "Change password" }));

    await waitFor(() =>
      expect(api.resetPassword).toHaveBeenCalledWith(
        "one-time-token",
        "new-password",
      ),
    );
    expect(
      await screen.findByText("Your password has been changed."),
    ).toBeInTheDocument();
  });

  it("does not submit when the password confirmation differs", async () => {
    const user = userEvent.setup();
    renderWithProviders(<ResetPassword />, {
      route: "/reset-password?token=one-time-token",
    });

    await user.type(screen.getByLabelText("New password"), "new-password");
    await user.type(
      screen.getByLabelText("Confirm new password"),
      "other-password",
    );
    await user.click(screen.getByRole("button", { name: "Change password" }));

    expect(await screen.findByText("Passwords do not match.")).toBeInTheDocument();
    expect(api.resetPassword).not.toHaveBeenCalled();
  });

  it("rejects a link with no token", () => {
    renderWithProviders(<ResetPassword />, { route: "/reset-password" });

    expect(
      screen.getByText(
        "This reset link is invalid. Ask an administrator for a new link.",
      ),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Change password" }),
    ).not.toBeInTheDocument();
  });
});
