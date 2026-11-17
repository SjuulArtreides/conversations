import { describe, expect, it, vi } from "vitest";
import { act, render } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import type { LiveSession } from "./types";
import { useLiveSwitchHotkey } from "./useLiveSwitchHotkey";

const navigate = vi.fn();
vi.mock("react-router-dom", async () => {
  const actual =
    await vi.importActual<typeof import("react-router-dom")>(
      "react-router-dom",
    );
  return { ...actual, useNavigate: () => navigate };
});

function liveSession(overrides: Partial<LiveSession> = {}): LiveSession {
  return {
    id: "sess-1",
    session_id: "sess-1",
    title: "Session one",
    project_path: null,
    started_at: null,
    last_event_at: null,
    mode: "interactive",
    viewer_is_author: true,
    viewer_can_write: true,
    ...overrides,
  };
}

function Harness({
  sessions,
  currentId,
  onCreate,
}: {
  sessions: LiveSession[];
  currentId: string;
  onCreate?: () => void;
}) {
  useLiveSwitchHotkey(sessions, currentId, { onCreate });
  return null;
}

function mount(
  sessions: LiveSession[],
  currentId: string,
  route = "/sessions/a",
  onCreate?: () => void,
) {
  return render(
    <MemoryRouter initialEntries={[route]}>
      <Harness sessions={sessions} currentId={currentId} onCreate={onCreate} />
    </MemoryRouter>,
  );
}

/** Press a key on `document` in the capture phase the hook listens on. */
function press(key: string, opts: KeyboardEventInit = {}) {
  act(() => {
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key, bubbles: true, ...opts }),
    );
  });
}

const two = [
  liveSession({ id: "a", title: "Alpha" }),
  liveSession({ id: "b", title: "Beta" }),
];

describe("useLiveSwitchHotkey", () => {
  it("Ctrl-A then a digit switches to that session", () => {
    navigate.mockClear();
    mount(two, "a");
    press("a", { ctrlKey: true });
    press("2");
    expect(navigate).toHaveBeenCalledWith("/sessions/b");
  });

  it("preserves ?fullscreen=1 when switching from a fullscreen URL", () => {
    navigate.mockClear();
    mount(two, "a", "/sessions/a?fullscreen=1");
    press("a", { ctrlKey: true });
    press("2");
    expect(navigate).toHaveBeenCalledWith("/sessions/b?fullscreen=1");
  });

  it("does not navigate to the already-current session", () => {
    navigate.mockClear();
    mount(two, "a");
    press("a", { ctrlKey: true });
    press("1"); // index 1 == current
    expect(navigate).not.toHaveBeenCalled();
  });

  it("is inert with fewer than two sessions", () => {
    navigate.mockClear();
    mount([liveSession({ id: "a" })], "a");
    press("a", { ctrlKey: true });
    press("2");
    expect(navigate).not.toHaveBeenCalled();
  });

  it("cancels the prefix on a non-digit key", () => {
    navigate.mockClear();
    mount(two, "a");
    press("a", { ctrlKey: true });
    press("x"); // cancels
    press("2"); // no longer armed ⇒ ignored
    expect(navigate).not.toHaveBeenCalled();
  });

  it("ignores Ctrl-A while a plain input is focused (native select-all wins)", () => {
    navigate.mockClear();
    mount(two, "a");
    const input = document.createElement("input");
    document.body.appendChild(input);
    input.focus();
    press("a", { ctrlKey: true });
    press("2");
    expect(navigate).not.toHaveBeenCalled();
    input.remove();
  });

  it("Ctrl-A c invokes onCreate", () => {
    const onCreate = vi.fn();
    mount(two, "a", "/sessions/a", onCreate);
    press("a", { ctrlKey: true });
    press("c");
    expect(onCreate).toHaveBeenCalledTimes(1);
  });

  it("arms for Ctrl-A c even with a single session when onCreate is given", () => {
    const onCreate = vi.fn();
    mount([liveSession({ id: "a" })], "a", "/sessions/a", onCreate);
    press("a", { ctrlKey: true });
    press("c");
    expect(onCreate).toHaveBeenCalledTimes(1);
  });

  it("leaves Ctrl-A and the next key to the browser with one session and no onCreate", () => {
    mount([liveSession({ id: "a" })], "a");
    const ctrlA = new KeyboardEvent("keydown", {
      key: "a",
      ctrlKey: true,
      bubbles: true,
      cancelable: true,
    });
    const next = new KeyboardEvent("keydown", {
      key: "1",
      bubbles: true,
      cancelable: true,
    });
    act(() => {
      document.dispatchEvent(ctrlA);
      document.dispatchEvent(next);
    });
    expect(ctrlA.defaultPrevented).toBe(false);
    expect(next.defaultPrevented).toBe(false);
  });
});
