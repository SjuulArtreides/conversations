import { beforeEach, expect, it, vi } from "vitest";
import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { activeHost } from "@/lib/hostPairing";
import { StartNewSessionButton } from "./HostActions";

vi.mock("@/lib/useControlAvailable", () => ({
  useControlAvailable: () => ({ available: false, isLoading: false }),
}));
vi.mock("@/lib/api", () => ({
  api: { getUserSettings: async () => ({ default_agent: "claude" }) },
  ApiError: class extends Error {},
}));

const laptop = "a".repeat(43);
const desktop = "b".repeat(43);

beforeEach(() => {
  localStorage.clear();
  localStorage.setItem(
    "conversations.hosts",
    JSON.stringify({
      hosts: [
        { id: laptop, label: "Laptop" },
        { id: desktop, label: "Desktop" },
      ],
      active: laptop,
    }),
  );
});

it("selects and remembers the named host beside the session button, even while offline", async () => {
  const user = userEvent.setup();
  const view = renderWithProviders(<StartNewSessionButton />);
  const selector = screen.getByRole("combobox", { name: "Session host" });
  expect(selector).toHaveValue(laptop);
  await user.selectOptions(selector, "Desktop");
  expect(activeHost()?.id).toBe(desktop);
  expect(selector).toHaveValue(desktop);
  view.unmount();
  renderWithProviders(<StartNewSessionButton label="New session" />);
  expect(screen.getByRole("combobox", { name: "Session host" })).toHaveValue(
    desktop,
  );
});
