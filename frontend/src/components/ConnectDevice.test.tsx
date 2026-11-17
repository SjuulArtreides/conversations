import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, useLocation } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";
import { ConnectDevice } from "./ConnectDevice";

vi.mock("@/routes/HostLink", () => ({
  parseHostLink: vi.fn(async (hash: string) =>
    hash === "#valid-setup" ? { challenge: "setup" } : null,
  ),
}));

function Location() {
  const location = useLocation();
  return (
    <output aria-label="Current page">
      {location.pathname + location.hash}
    </output>
  );
}

describe("ConnectDevice", () => {
  it("continues to the existing enrollment page with the validated setup fragment", async () => {
    render(
      <MemoryRouter>
        <ConnectDevice />
        <Location />
      </MemoryRouter>,
    );
    await userEvent.click(
      screen.getByRole("button", { name: "Connect this device" }),
    );
    await userEvent.type(
      screen.getByRole("textbox", { name: "Setup link" }),
      `${window.location.origin}/host-link#valid-setup`,
    );
    await userEvent.click(
      screen.getByRole("button", { name: "Continue to passkey setup" }),
    );
    expect(await screen.findByLabelText("Current page")).toHaveTextContent(
      "/host-link#valid-setup",
    );
  });
});
