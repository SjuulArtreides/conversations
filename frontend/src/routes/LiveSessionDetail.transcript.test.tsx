import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as React from "react";
import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "@/test/utils";
import { __resetLiveCacheForTest } from "@/lib/liveMessageCache";
import type { AgentKind, LiveSession, Message } from "@/lib/types";
import { LiveSessionDetail } from "./LiveSessionDetail";

// Unlike LiveSessionDetail.test.tsx this suite keeps the real live-stream hook
// and observes the requests it makes, so it proves what the collapsed
// transcript downloads rather than which props the route passes around.


vi.mock("@/components/PrActivityPanel", () => ({
  PrActivityPanel: () => null,
}));

vi.mock("@/lib/useLiveSessions", () => ({
  useLiveSessions: vi.fn(),
  LIVE_SESSIONS_QUERY_KEY: ["live-sessions"],
}));

const route = vi.hoisted(() => ({ id: "sess-1" }));

vi.mock("react-router-dom", async () => {
  const actual =
    await vi.importActual<typeof import("react-router-dom")>(
      "react-router-dom",
    );
  return { ...actual, useParams: () => ({ id: route.id }) };
});

import { useLiveSessions } from "@/lib/useLiveSessions";

function body(uuid: string, role: "user" | "assistant", text: string): Message {
  return {
    uuid,
    parent_uuid: null,
    role,
    kind: "text",
    model: null,
    is_sidechain: false,
    timestamp: null,
    blocks: [{ type: "text", text }],
    text,
    usage: null,
  };
}

const MESSAGES = [
  body("m0", "user", "Bootstrap instructions for the agent"),
  body("m1", "assistant", "Historical assistant answer"),
];

const OUTLINE = [
  { type: "message_outline", seq: 0, role: "user", visible: true },
  { type: "message_outline", seq: 1, role: "assistant", visible: true },
  // A tool-result-only message: folded away, so not counted.
  { type: "message_outline", seq: 2, role: "user", visible: false },
];

function sse(frames: object[]): Response {
  const encoder = new TextEncoder();
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const frame of frames) {
        controller.enqueue(
          encoder.encode(`data: ${JSON.stringify(frame)}\n\n`),
        );
      }
      // Stay open like a real live stream so the hook never reconnects.
    },
  });
  return new Response(stream, {
    headers: { "content-type": "text/event-stream" },
  });
}

// Resolves each session's in-flight reply POST, keyed by session id.
let answerReply: Record<string, (delivered: boolean) => void> = {};

// Replies are signed host operations now; "undelivered" is the host failing.
vi.mock("@/lib/hostControl", async () => {
  const actual =
    await vi.importActual<typeof import("@/lib/hostControl")>(
      "@/lib/hostControl",
    );
  return {
    ...actual,
    hostControl: {
      ...actual.hostControl,
      sendReply: (id: string) =>
        new Promise<void>((resolve, reject) => {
          answerReply[id] = (delivered) =>
            delivered
              ? resolve()
              : reject(
                  new actual.HostControlError(
                    "No live CLI attached — reply not delivered.",
                    "failed",
                  ),
                );
        }),
    },
  };
});

const fetchMock = vi.fn((input: RequestInfo | URL) => {
  const url = String(input);
  const { pathname, searchParams } = new URL(url, "http://localhost");
  if (/^\/api\/live\/sess-\d\/stream$/.test(pathname)) {
    // Like the server: replay only what follows `since`, as outline frames
    // when `bodies=false`, whatever the query order.
    const since = Number(searchParams.get("since") ?? -1);
    const frames =
      searchParams.get("bodies") === "false"
        ? OUTLINE
        : MESSAGES.map((message, seq) => ({ type: "message", seq, message }));
    return Promise.resolve(sse(frames.filter((frame) => frame.seq > since)));
  }
  return Promise.resolve(
    new Response("[]", { headers: { "content-type": "application/json" } }),
  );
});

function streamRequests(id = "sess-1"): string[] {
  return fetchMock.mock.calls
    .map(([input]) => String(input))
    .filter((url) => url.startsWith(`/api/live/${id}/stream`));
}

/** Changes the route id the mocked `useParams` reports, then re-renders. */
function RouteSwitcher() {
  const [, rerender] = React.useReducer((n: number) => n + 1, 0);
  const go = (id: string) => {
    route.id = id;
    rerender();
  };
  return (
    <>
      <button onClick={() => go("sess-1")}>go sess-1</button>
      <button onClick={() => go("sess-2")}>go sess-2</button>
      <LiveSessionDetail />
    </>
  );
}

function setup(agent: AgentKind = "claude") {
  const session = (id: string): LiveSession => ({
    id,
    session_id: id,
    title: "My live session",
    project_path: null,
    started_at: null,
    last_event_at: null,
    mode: "interactive",
    agent,
    viewer_is_author: true,
    viewer_can_write: true,
  });
  vi.mocked(useLiveSessions).mockReturnValue({
    list: [session("sess-1"), session("sess-2")],
    activeIds: new Set(["sess-1", "sess-2"]),
    isLoading: false,
  });
}

beforeEach(() => {
  route.id = "sess-1";
  answerReply = {};
  __resetLiveCacheForTest();
  fetchMock.mockClear();
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe("LiveSessionDetail full conversation", () => {
  it.each<AgentKind>(["claude", "codex"])(
    "starts collapsed for %s and downloads only the message outline",
    async (agent) => {
      setup(agent);
      renderWithProviders(<LiveSessionDetail />);

      expect(
        await screen.findByRole("button", {
          name: /show full conversation \(2\)/i,
        }),
      ).toHaveAttribute("aria-expanded", "false");
      expect(screen.queryByText("Historical assistant answer")).toBeNull();
      expect(screen.queryByRole("textbox", { name: "Reply" })).toBeNull();
      expect(streamRequests()).toEqual([
        "/api/live/sess-1/stream?bodies=false",
      ]);
    },
  );

  it("loads message bodies only when expanded", async () => {
    setup();
    const user = userEvent.setup();
    renderWithProviders(<LiveSessionDetail />);

    await user.click(
      await screen.findByRole("button", {
        name: /show full conversation \(2\)/i,
      }),
    );

    expect(
      await screen.findByText("Historical assistant answer"),
    ).toBeInTheDocument();
    expect(streamRequests()).toEqual([
      "/api/live/sess-1/stream?bodies=false",
      "/api/live/sess-1/stream",
    ]);
  });


  it("is collapsed again when returning to a session that was open", async () => {
    setup();
    const user = userEvent.setup();
    renderWithProviders(<RouteSwitcher />);
    await user.click(
      await screen.findByRole("button", { name: /show full conversation/i }),
    );
    await screen.findByText("Historical assistant answer");

    await user.click(screen.getByRole("button", { name: "go sess-2" }));
    await user.click(screen.getByRole("button", { name: "go sess-1" }));

    expect(
      await screen.findByRole("button", { name: /show full conversation/i }),
    ).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByText("Historical assistant answer")).toBeNull();
    expect(streamRequests("sess-1").at(-1)).toMatch(/bodies=false$/);
  });



});
