import { afterEach, describe, expect, it, vi } from "vitest";
import { saveHostLinkForLogin, takeHostLinkAfterLogin } from "./hostLinkReturn";
afterEach(() => {
  sessionStorage.clear();
  vi.useRealTimers();
});
describe("host link sign in", () => {
  it("keeps the fragment locally for a single return after sign in", () => {
    saveHostLinkForLogin("#desc=fixture&challenge=fixture&options=fixture");
    expect(takeHostLinkAfterLogin()).toBe(
      "#desc=fixture&challenge=fixture&options=fixture",
    );
    expect(takeHostLinkAfterLogin()).toBeNull();
  });
  it("does not return an external URL or an expired ceremony", () => {
    saveHostLinkForLogin("https://evil.test/");
    expect(takeHostLinkAfterLogin()).toBeNull();
    vi.useFakeTimers();
    saveHostLinkForLogin("#desc=fixture");
    vi.advanceTimersByTime(600_001);
    expect(takeHostLinkAfterLogin()).toBeNull();
  });
});
