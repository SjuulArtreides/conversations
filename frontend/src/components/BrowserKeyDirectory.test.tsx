import "fake-indexeddb/auto";
import { beforeEach, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { BrowserKeyDirectory } from "./BrowserKeyDirectory";
import { loadBrowserKey, fromBase64Url } from "@/lib/hostAuth";
const api = vi.hoisted(() => ({
  userKeyDirectory: vi.fn(),
  browserKeyChallenge: vi.fn(),
  publishBrowserKey: vi.fn(),
}));
vi.mock("@/lib/api", () => ({ api }));
vi.mock("@/lib/useAuth", () => ({
  useAuth: () => ({ user: { email: "alice@example.com" } }),
}));
beforeEach(() => {
  vi.clearAllMocks();
  api.userKeyDirectory.mockResolvedValue([
    {
      email: "bob@example.com",
      display_name: "Bob",
      keys: [
        {
          kid: "B".repeat(43),
          alg: "es256",
          public_key: "public",
          label: "Bob laptop",
          created_at: "2026-11-05",
        },
      ],
    },
  ]);
  api.publishBrowserKey.mockResolvedValue({});
});
it("shows published users and only publishes this browser after explicit action", async () => {
  const key = await loadBrowserKey();
  let payload = "";
  api.browserKeyChallenge.mockImplementation(
    async (public_key: string, label: string) => {
      payload = JSON.stringify({
        v: 1,
        purpose: "publish_browser_key",
        user_email: "alice@example.com",
        public_key,
        kid: key.kid,
        label,
        challenge: "a".repeat(32),
        expires_at: Math.floor(Date.now() / 1000) + 300,
      });
      return { challenge: "a".repeat(32), payload };
    },
  );
  renderWithProviders(<BrowserKeyDirectory />);
  expect(await screen.findByText(key.kid)).toBeInTheDocument();
  expect(await screen.findByText("bob@example.com")).toBeInTheDocument();
  expect(screen.getByText("B".repeat(43))).toBeInTheDocument();
  expect(api.publishBrowserKey).not.toHaveBeenCalled();
  const user = userEvent.setup();
  const name = screen.getByRole("textbox", { name: "Browser name" });
  await user.clear(name);
  await user.type(name, "Work browser");
  expect(name).toHaveValue("Work browser");
  await userEvent.click(
    screen.getByRole("button", { name: "Publish current browser key" }),
  );
  await waitFor(() => expect(api.publishBrowserKey).toHaveBeenCalledOnce());
  expect(api.browserKeyChallenge).toHaveBeenCalledWith(
    expect.any(String),
    "Work browser",
  );
  const [challenge, signature] = api.publishBrowserKey.mock.calls[0];
  expect(challenge).toBe("a".repeat(32));
  const publicKey = await crypto.subtle.importKey(
    "raw",
    key.publicRaw,
    { name: "ECDSA", namedCurve: "P-256" },
    true,
    ["verify"],
  );
  expect(
    await crypto.subtle.verify(
      { name: "ECDSA", hash: "SHA-256" },
      publicKey,
      fromBase64Url(signature),
      new TextEncoder().encode("conversations-browser-key/v1\n" + payload),
    ),
  ).toBe(true);
  expect(
    await screen.findByText(/Published\. The host owner/),
  ).toBeInTheDocument();
});

it("shows a publication error and lets the user edit the name and retry", async () => {
  const user = userEvent.setup();
  const key = await loadBrowserKey();
  api.browserKeyChallenge.mockRejectedValue(
    new Error("Unsupported Media Type"),
  );
  renderWithProviders(<BrowserKeyDirectory />);
  await screen.findByText(key.kid);
  await user.click(
    screen.getByRole("button", { name: "Publish current browser key" }),
  );
  expect(await screen.findByRole("status")).toHaveTextContent(
    "Unsupported Media Type",
  );
  const name = screen.getByRole("textbox", { name: "Browser name" });
  await user.clear(name);
  await user.type(name, "Retry browser");
  expect(name).toHaveValue("Retry browser");
  expect(
    screen.getByRole("button", { name: "Publish current browser key" }),
  ).toBeEnabled();
});

it("can show fingerprints and directory search without duplicate publication controls", async () => {
  const user = userEvent.setup();
  const key = await loadBrowserKey();
  renderWithProviders(<BrowserKeyDirectory showPublication={false} />);
  expect(await screen.findByText(key.kid)).toBeVisible();
  expect(await screen.findByText("bob@example.com")).toBeVisible();
  expect(
    screen.queryByRole("textbox", { name: "Browser name" }),
  ).not.toBeInTheDocument();
  expect(
    screen.queryByRole("button", { name: "Publish current browser key" }),
  ).not.toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Copy my fingerprint" }));
  expect(await screen.findByRole("status")).toHaveTextContent(
    "Fingerprint copied.",
  );
  await user.type(screen.getByRole("textbox", { name: "Search users" }), "bob");
  await waitFor(() => expect(api.userKeyDirectory).toHaveBeenCalledWith("bob"));
  expect(api.browserKeyChallenge).not.toHaveBeenCalled();
  expect(api.publishBrowserKey).not.toHaveBeenCalled();
});
