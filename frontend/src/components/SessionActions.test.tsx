import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { Button } from "@/components/ui/button";
import { SessionActions } from "./SessionActions";

describe("SessionActions", () => {
  const gated = (() => false)();
  it("puts mobile actions behind an accessible collapsible", async () => {
    const user = userEvent.setup();
    renderWithProviders(
      <SessionActions>
        <Button>Rename</Button>
        <Button>Share</Button>
      </SessionActions>,
    );

    const toggle = screen.getByRole("button", { name: "Session actions" });
    const actions = document.getElementById(
      toggle.getAttribute("aria-controls") ?? "",
    );
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(actions).toHaveClass("hidden");

    await user.click(toggle);

    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(actions).not.toHaveClass("hidden");
    expect(screen.getByRole("button", { name: "Rename" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Share" })).toBeInTheDocument();
  });

  it("renders nothing when every child is a conditional that resolved to nothing", () => {
    renderWithProviders(
      <SessionActions>
        {gated && <Button>Rename</Button>}
        {null}
        {undefined}
      </SessionActions>,
    );

    expect(
      screen.queryByRole("button", { name: "Session actions" }),
    ).not.toBeInTheDocument();
    expect(screen.queryByRole("button")).not.toBeInTheDocument();
  });

  it("keeps the toggle while any child still renders", () => {
    renderWithProviders(
      <SessionActions>
        {gated && <Button>Rename</Button>}
        <Button>Close</Button>
      </SessionActions>,
    );

    expect(
      screen.getByRole("button", { name: "Session actions" }),
    ).toBeInTheDocument();
  });
});
