import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { Login } from "./Login";

vi.mock("@/lib/terminalCache", () => ({
  enableTerminalCache: vi.fn(),
}));

// Mock the API the page (via useAuth) depends on. Spread the real module so
// ApiError and the query builders keep working.
vi.mock("@/lib/api", async (orig) => {
  const actual = await (orig as () => Promise<typeof import("@/lib/api")>)();
  return {
    ...actual,
    api: {
      authConfig: vi.fn().mockResolvedValue({
        google_enabled: false,
        password_enabled: true,
        user: null,
      }),
      login: vi
        .fn()
        .mockResolvedValue({ email: "dev@example.com", display_name: "Dev" }),
      register: vi.fn(),
      requestSignupEmail: vi.fn().mockResolvedValue({ ok: true }),
    },
  };
});

import { api } from "@/lib/api";
import { enableTerminalCache } from "@/lib/terminalCache";

function authFlags(flags: {
  google_enabled: boolean;
  password_enabled: boolean;
  signup_verification_enabled?: boolean;
}) {
  vi.mocked(api.authConfig).mockResolvedValue({ ...flags, user: null });
}

beforeEach(() => {
  vi.clearAllMocks();
  authFlags({ google_enabled: false, password_enabled: true });
});

const isPrimary = (el: Element) => el.classList.contains("bg-primary");

describe("Login", () => {
  it("requests a verification email before collecting a signup password", async () => {
    authFlags({
      google_enabled: false,
      password_enabled: true,
      signup_verification_enabled: true,
    });
    const user = userEvent.setup();
    renderWithProviders(<Login />, { route: "/login" });

    await user.click(await screen.findByRole("button", { name: "Create account" }));
    expect(screen.queryByLabelText("Password")).not.toBeInTheDocument();
    await user.type(screen.getByLabelText("Email"), "dev@example.com");
    await user.click(screen.getByRole("button", { name: "Send verification email" }));

    expect(await screen.findByRole("status")).toHaveTextContent("Check your inbox");
    expect(api.requestSignupEmail).toHaveBeenCalledWith("dev@example.com");
    expect(api.register).not.toHaveBeenCalled();
  });

  it("submits the password login form with the entered credentials", async () => {
    const user = userEvent.setup();
    const { container } = renderWithProviders(<Login />, { route: "/login" });

    // Wait for the auth-config probe to resolve and the form to render.
    const email = await screen.findByLabelText("Email");
    const password = screen.getByLabelText("Password");
    await user.type(email, "dev@example.com");
    await user.type(password, "hunter2pass");

    const submit = container.querySelector(
      'button[type="submit"]',
    ) as HTMLButtonElement;
    await user.click(submit);

    await waitFor(() =>
      expect(api.login).toHaveBeenCalledWith({
        email: "dev@example.com",
        password: "hunter2pass",
      }),
    );
    expect(enableTerminalCache).toHaveBeenCalledOnce();
  });

  it("puts Google SSO first and primary when both login methods are enabled", async () => {
    authFlags({ google_enabled: true, password_enabled: true });
    const user = userEvent.setup();
    const { container } = renderWithProviders(<Login />, { route: "/login" });

    const google = await screen.findByRole("link", { name: /google/i });
    expect(google).toHaveAttribute("href", "/auth/google/login");

    // Keyboard users reach SSO before anything in the password path.
    await user.tab();
    expect(google).toHaveFocus();

    // SSO is the primary action; the password submit is visibly secondary.
    const submit = container.querySelector('button[type="submit"]')!;
    expect(isPrimary(google)).toBe(true);
    expect(isPrimary(submit)).toBe(false);
  });

  it("keeps password login working alongside SSO so accounts can be linked", async () => {
    authFlags({ google_enabled: true, password_enabled: true });
    const user = userEvent.setup();
    renderWithProviders(<Login />, { route: "/login" });

    await user.type(await screen.findByLabelText("Email"), "dev@example.com");
    await user.type(screen.getByLabelText("Password"), "hunter2pass");
    await user.keyboard("{Enter}");

    await waitFor(() =>
      expect(api.login).toHaveBeenCalledWith({
        email: "dev@example.com",
        password: "hunter2pass",
      }),
    );
  });

  it("shows only the primary Google action when password login is disabled", async () => {
    authFlags({ google_enabled: true, password_enabled: false });
    renderWithProviders(<Login />, { route: "/login" });

    const google = await screen.findByRole("link", { name: /google/i });
    expect(isPrimary(google)).toBe(true);
    expect(screen.queryByLabelText("Email")).not.toBeInTheDocument();
  });

  it("keeps the password submit primary when SSO is disabled", async () => {
    const { container } = renderWithProviders(<Login />, { route: "/login" });

    await screen.findByLabelText("Email");
    expect(isPrimary(container.querySelector('button[type="submit"]')!)).toBe(
      true,
    );
    expect(screen.queryByRole("link", { name: /google/i })).toBeNull();
  });
});
