import { beforeEach, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import * as React from "react";
import { useLiveStream } from "@/lib/useLiveStream";
import { renderMermaidSvg } from "@/lib/mermaid";
import type { Message } from "@/lib/types";
import { TerminalDiagrams } from "./TerminalDiagrams";

vi.mock("@/lib/useLiveStream", () => ({ useLiveStream: vi.fn() }));
vi.mock("@/lib/mermaid", async () => ({
  ...(await vi.importActual("@/lib/mermaid")),
  renderMermaidSvg: vi.fn(),
}));
const source = "flowchart LR\n A[Request received] --> B[Complete]";
const messages = [
  {
    uuid: "answer",
    role: "assistant",
    kind: "text",
    blocks: [{ type: "text", text: "```mermaid\n" + source + "\n```" }],
  },
] as Message[];

beforeEach(() => {
  vi.mocked(useLiveStream).mockReset();
  vi.mocked(useLiveStream).mockReturnValue({
    messages,
    outline: { count: 1, lastRole: "assistant" },
    status: { kind: "streaming" },
  });
  vi.mocked(renderMermaidSvg).mockReset();
  vi.mocked(renderMermaidSvg).mockResolvedValue(
    '<svg viewBox="0 0 2400 400"></svg>',
  );
});

/**
 * The diagrams viewer is opened from the terminal's Display menu; tests here
 * stand in for that menu with a plain button so the dialog's behavior stays
 * exercised in isolation. The LiveTerminal tests cover the real menu wiring.
 */
function Harness({
  sessionId,
  hostId,
}: {
  sessionId: string;
  hostId?: string;
}) {
  const [open, setOpen] = React.useState(false);
  return (
    <>
      <button type="button" onClick={() => setOpen(true)}>
        View terminal diagrams
      </button>
      <TerminalDiagrams
        sessionId={sessionId}
        hostId={hostId}
        open={open}
        onOpenChange={setOpen}
      />
    </>
  );
}

function mount() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const ui = () => (
    <QueryClientProvider client={client}>
      <Harness sessionId="terminal-session" />
    </QueryClientProvider>
  );
  const view = render(ui());
  return { ...view, refresh: () => view.rerender(ui()) };
}

it("defers requesting live messages until the viewer opens, then renders the diagram", async () => {
  const user = userEvent.setup();
  mount();
  expect(useLiveStream).not.toHaveBeenCalled();
  await user.click(
    screen.getByRole("button", { name: "View terminal diagrams" }),
  );
  expect(
    screen.getByRole("dialog", { name: "Terminal diagrams" }),
  ).toBeInTheDocument();
  expect(await screen.findByTitle("Expanded Mermaid diagram")).toHaveAttribute(
    "sandbox",
    "",
  );
  expect(renderMermaidSvg).toHaveBeenCalledWith(expect.any(String), source);
  expect(useLiveStream).toHaveBeenCalledWith("terminal-session");
  await user.click(screen.getByRole("button", { name: "Actual size" }));
  expect(screen.getByTitle("Expanded Mermaid diagram")).toHaveStyle({
    width: "2400px",
  });
  await user.keyboard("{Escape}");
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
});

it("shows an honest empty state while the agent has not supplied Mermaid source", async () => {
  vi.mocked(useLiveStream).mockReturnValue({
    messages: [],
    outline: { count: 0, lastRole: undefined },
    status: { kind: "streaming" },
  });
  mount();
  await userEvent
    .setup()
    .click(screen.getByRole("button", { name: "View terminal diagrams" }));
  expect(screen.getByRole("status")).toHaveTextContent(
    "No complete Mermaid diagrams yet",
  );
  expect(renderMermaidSvg).not.toHaveBeenCalled();
});

it("shows parser failures and lets the user retry", async () => {
  vi.mocked(renderMermaidSvg).mockRejectedValueOnce(
    new Error("Parse error on line 2"),
  );
  mount();
  const user = userEvent.setup();
  await user.click(
    screen.getByRole("button", { name: "View terminal diagrams" }),
  );
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Parse error on line 2",
  );
  await user.click(screen.getByRole("button", { name: "Retry" }));
  expect(
    await screen.findByTitle("Expanded Mermaid diagram"),
  ).toBeInTheDocument();
});

it("picks up streamed diagrams and lets the reader choose an earlier diagram", async () => {
  vi.mocked(useLiveStream).mockReturnValue({
    messages: [],
    outline: { count: 0, lastRole: undefined },
    status: { kind: "streaming" },
  });
  const { refresh } = mount();
  const user = userEvent.setup();
  await user.click(
    screen.getByRole("button", { name: "View terminal diagrams" }),
  );
  const next = "flowchart TD\n B --> C";
  vi.mocked(useLiveStream).mockReturnValue({
    messages: [
      ...messages,
      {
        ...messages[0],
        uuid: "next-answer",
        blocks: [{ type: "text", text: "```mermaid\n" + next + "\n```" }],
      },
    ],
    outline: { count: 2, lastRole: "assistant" },
    status: { kind: "streaming" },
  });
  refresh();
  await waitFor(() =>
    expect(renderMermaidSvg).toHaveBeenCalledWith(expect.any(String), next),
  );
  await user.selectOptions(
    screen.getByRole("combobox", { name: "Choose diagram" }),
    "answer:0",
  );
  await waitFor(() =>
    expect(renderMermaidSvg).toHaveBeenCalledWith(expect.any(String), source),
  );
});

it("renders a private diagram from the first page and cancels the remaining history when closed", async () => {
  const { hostControl } = await import("@/lib/hostControl");
  const privateChannel = await import("@/lib/privateChannel");
  let signal: AbortSignal | undefined;
  const read = vi
    .spyOn(privateChannel, "readHistory")
    .mockImplementation(async (_channel, _id, options) => {
      const opts = typeof options === "object" ? options : {};
      signal = opts.signal;
      opts.onPage?.([
        { seq: 1, sha256: "hash", message: messages[0] } as never,
      ]);
      return new Promise(() => {});
    });
  const host = vi
    .spyOn(hostControl, "privateRead")
    .mockImplementation(async (_id, callback) => callback({} as never));
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const mounted = render(
    <QueryClientProvider client={client}>
      <Harness sessionId="s" hostId="host-a" />
    </QueryClientProvider>,
  );
  expect(read).not.toHaveBeenCalled();
  await userEvent.click(
    screen.getByRole("button", { name: "View terminal diagrams" }),
  );
  await waitFor(() =>
    expect(renderMermaidSvg).toHaveBeenCalledWith(expect.any(String), source),
  );
  expect(useLiveStream).not.toHaveBeenCalled();
  mounted.unmount();
  expect(signal?.aborted).toBe(true);
  read.mockRestore();
  host.mockRestore();
});
