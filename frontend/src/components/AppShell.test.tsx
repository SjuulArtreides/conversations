import { afterEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { AppShell } from "./AppShell";
import { renderWithProviders } from "@/test/utils";
import { api } from "@/lib/api";
import { suspendTerminalCache } from "@/lib/terminalCache";

vi.mock("@/lib/terminalCache", () => ({
  suspendTerminalCache: vi.fn().mockResolvedValue(undefined),
}));

describe("AppShell", () => {
  afterEach(() => vi.restoreAllMocks());

  it("gives session pages a wider aligned shell without widening the session list", () => {
    const { container, unmount } = renderWithProviders(
      <AppShell>
        <div>Session content</div>
      </AppShell>,
      { route: "/sessions/preview" },
    );
    expect(container.querySelector("header > div")).toHaveClass(
      "max-w-[112rem]",
    );
    expect(container.querySelector("main")).toHaveClass("max-w-[112rem]");
    expect(
      screen.queryByRole("button", { name: /layout/i }),
    ).not.toBeInTheDocument();

    unmount();
    const list = renderWithProviders(
      <AppShell>
        <div>Sessions</div>
      </AppShell>,
    );
    expect(list.container.querySelector("header > div")).toHaveClass(
      "max-w-5xl",
    );
    expect(list.container.querySelector("main")).toHaveClass("max-w-5xl");
  });

  it("shows the active app version in the header", async () => {
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url === "/api/app") {
        return {
          ok: true,
          json: () => Promise.resolve({ version: "0.8.1" }),
        };
      }
      if (url === "/api/auth/config") {
        return {
          ok: false,
          status: 404,
          statusText: "Not Found",
          json: () => Promise.resolve({ error: "auth disabled" }),
        };
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal("fetch", fetchMock);

    renderWithProviders(
      <AppShell>
        <div>Sessions</div>
      </AppShell>,
    );

    expect(
      await screen.findByLabelText("Conversations version 0.8.1"),
    ).toHaveTextContent("v0.8.1");
  });

  it("puts the mobile navigation links behind a menu", async () => {
    const user = userEvent.setup();

    renderWithProviders(
      <AppShell>
        <div>Page content</div>
      </AppShell>,
    );

    const trigger = screen.getByRole("button", {
      name: "Open navigation menu",
    });
    expect(trigger).toHaveAttribute("aria-expanded", "false");

    await user.click(trigger);

    expect(trigger).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByRole("menuitem", { name: "Sessions" })).toHaveAttribute(
      "href",
      "/",
    );
    expect(screen.getByRole("menuitem", { name: "Projects" })).toHaveAttribute(
      "href",
      "/projects",
    );
    // Settings is reached through the always-visible gear, not the mobile menu.
    expect(
      screen.queryByRole("menuitem", { name: "Settings" }),
    ).not.toBeInTheDocument();

    await user.keyboard("{Escape}");
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
  });

  it("keeps a Settings gear link visible in the header on every width", () => {
    const { container } = renderWithProviders(
      <AppShell>
        <div>Page content</div>
      </AppShell>,
    );

    // There is exactly ONE Settings control — the gear link — and it is a
    // top-level link in the header (not hidden inside the mobile menu or any
    // desktop-only nav), so it stays reachable on narrow viewports and from
    // routes that don't surface the nav row.
    const gears = screen.getAllByRole("link", { name: "Settings" });
    expect(gears).toHaveLength(1);
    expect(gears[0]).toHaveAttribute("href", "/settings");
    expect(gears[0]).toHaveAttribute("title", "Settings");

    // The gear is the RIGHTMOST interactive control in the header — it comes
    // after the notifications bell AND after both the desktop auth corner
    // (Sign out) and the mobile hamburger menu. Walk every interactive
    // header control in document order; the gear must be last.
    const header = container.querySelector("header")!;
    const interactive = Array.from(
      header.querySelectorAll<HTMLElement>(
        'a[href], button:not([aria-hidden="true"])',
      ),
    );
    expect(interactive[interactive.length - 1]).toBe(gears[0]);
  });

  it("shows Sign in in the mobile menu when no user is signed in", async () => {
    vi.spyOn(api, "authConfig").mockResolvedValue({
      google_enabled: false,
      password_enabled: true,
      user: null,
    });
    const user = userEvent.setup();

    renderWithProviders(
      <AppShell>
        <div>Page content</div>
      </AppShell>,
    );

    await user.click(
      await screen.findByRole("button", { name: "Open navigation menu" }),
    );
    expect(screen.getByRole("menuitem", { name: "Sign in" })).toHaveAttribute(
      "href",
      "/login",
    );
  });

  it("shows Sign out and reports a failed sign-out request", async () => {
    vi.spyOn(api, "authConfig").mockResolvedValue({
      google_enabled: false,
      password_enabled: true,
      user: { email: "dev@example.com", display_name: "Dev" },
    });
    vi.spyOn(api, "logout").mockRejectedValue(new Error("Network unavailable"));
    const user = userEvent.setup();

    renderWithProviders(
      <AppShell>
        <div>Page content</div>
      </AppShell>,
    );

    await user.click(
      await screen.findByRole("button", { name: "Open navigation menu" }),
    );
    await user.click(screen.getByRole("menuitem", { name: "Sign out" }));

    await waitFor(() => expect(api.logout).toHaveBeenCalledOnce());
    expect(
      await screen.findByText("Could not sign out: Network unavailable"),
    ).toBeInTheDocument();
  });

  it("clears cached terminal contents after signing out", async () => {
    vi.spyOn(api, "authConfig").mockResolvedValue({
      google_enabled: false,
      password_enabled: true,
      user: { email: "dev@example.com", display_name: "Dev" },
    });
    let resolveLogout!: () => void;
    vi.spyOn(api, "logout").mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          resolveLogout = resolve;
        }),
    );
    const user = userEvent.setup();

    renderWithProviders(
      <AppShell>
        <div>Page content</div>
      </AppShell>,
    );

    await user.click(await screen.findByRole("button", { name: "Sign out" }));

    await waitFor(() => expect(api.logout).toHaveBeenCalledOnce());
    expect(suspendTerminalCache).not.toHaveBeenCalled();

    resolveLogout();
    await waitFor(() => expect(suspendTerminalCache).toHaveBeenCalledOnce());
  });
});
