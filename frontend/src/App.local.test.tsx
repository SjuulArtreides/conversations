import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import { renderWithProviders } from "@/test/utils";
import App from "./App";
import { api } from "@/lib/api";
import { useLocalHost } from "@/lib/localHost";

vi.mock("@/lib/localHost", () => ({
  useLocalHost: vi.fn(),
  refreshLocalHost: vi.fn(),
}));
vi.mock("@/lib/api", () => ({
  api: {
    authConfig: vi.fn(),
    appInfo: vi.fn(),
    notifications: vi.fn(),
    logout: vi.fn(),
  },
  ApiError: class extends Error {},
}));
vi.mock("@/components/Notifications", () => ({ Notifications: () => null }));
vi.mock("@/routes/SessionList", () => ({
  SessionList: () => <div>Session data loaded</div>,
}));
vi.mock("@/routes/SessionRoute", () => ({
  SessionRoute: () => <div>Session detail loaded</div>,
}));
vi.mock("@/routes/ProjectList", () => ({
  ProjectList: () => <div>Shared project list</div>,
}));
vi.mock("@/routes/ProjectDetail", () => ({
  ProjectDetail: () => <div>Shared project detail</div>,
}));
vi.mock("@/routes/Settings", () => ({
  Settings: () => <div>Settings data loaded</div>,
}));
vi.mock("@/routes/Login", () => ({ Login: () => <div>Hosted sign in</div> }));
vi.mock("@/routes/ResetPassword", () => ({ ResetPassword: () => null }));
vi.mock("@/routes/HostLink", () => ({ HostLink: () => null }));
vi.mock("@/routes/PrivateSessions", () => ({
  PrivateSessionList: () => null,
  PrivateSessionView: () => null,
}));

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(useLocalHost).mockReturnValue({
    local: false,
    authenticated: false,
    error: null,
  });
  vi.mocked(api.authConfig).mockResolvedValue({
    user: null,
    google_enabled: true,
    password_enabled: false,
  });
  vi.mocked(api.appInfo).mockResolvedValue({ version: "test" });
});

describe("shared application local owner gate", () => {
  it("keeps the shared shell but loads no local data without owner authentication", async () => {
    vi.mocked(useLocalHost).mockReturnValue({
      local: true,
      authenticated: false,
      error: "Owner session expired",
    });
    renderWithProviders(<App />, { route: "/settings" });
    expect(
      await screen.findByText("Connect this browser once"),
    ).toBeInTheDocument();
    expect(screen.getByText("Arrakis Conversations")).toBeInTheDocument();
    expect(screen.getByText("Owner session expired")).toBeInTheDocument();
    expect(screen.queryByText("Settings data loaded")).not.toBeInTheDocument();
    expect(api.authConfig).not.toHaveBeenCalled();
    expect(api.appInfo).not.toHaveBeenCalled();
  });
  it("uses the same routes for an authenticated local owner without remote OAuth", async () => {
    vi.mocked(useLocalHost).mockReturnValue({
      local: true,
      authenticated: true,
      error: null,
    });
    renderWithProviders(<App />);
    expect(await screen.findByText("Session data loaded")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Settings" })).toBeInTheDocument();
    expect(api.authConfig).not.toHaveBeenCalled();
    expect(
      screen.queryByRole("link", { name: "Sign in" }),
    ).not.toBeInTheDocument();
  });
  it.each([
    ["/projects", "Shared project list"],
    ["/projects/p1", "Shared project detail"],
  ])(
    "renders the shared project route %s for the local owner",
    async (route, label) => {
      vi.mocked(useLocalHost).mockReturnValue({
        local: true,
        authenticated: true,
        error: null,
      });
      renderWithProviders(<App />, { route });
      expect(await screen.findByText(label)).toBeInTheDocument();
      expect(api.authConfig).not.toHaveBeenCalled();
    },
  );
  it("preserves hosted authentication and redirects unauthenticated hosted routes", async () => {
    renderWithProviders(<App />);
    expect(await screen.findByText("Hosted sign in")).toBeInTheDocument();
    await waitFor(() => expect(api.authConfig).toHaveBeenCalled());
    expect(screen.queryByText("Session data loaded")).not.toBeInTheDocument();
  });
});
