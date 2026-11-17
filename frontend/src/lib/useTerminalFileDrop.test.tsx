import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ReactNode } from "react";
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { ToastProvider } from "@/components/ui/toast";
import { HostControlError } from "@/lib/hostControl";
import {
  TERMINAL_DROP_MAX_BYTES,
  TERMINAL_DROP_MAX_FILES,
  useTerminalFileDrop,
} from "./useTerminalFileDrop";

const { mockSendFiles, mockFetch } = vi.hoisted(() => ({
  mockSendFiles: vi.fn(),
  mockFetch: vi.fn(),
}));
vi.mock("@/lib/hostControl", async () => {
  const actual =
    await vi.importActual<typeof import("@/lib/hostControl")>(
      "@/lib/hostControl",
    );
  return { ...actual, hostControl: { sendFiles: mockSendFiles } };
});

// The wrapper option keeps the provider mounted across `rerender`, so a prop
// change updates the hook instead of remounting it.
function renderWithProviders(ui: ReactNode) {
  return render(ui, {
    wrapper: ({ children }) => <ToastProvider>{children}</ToastProvider>,
  });
}

function Harness({
  sessionId = "sess-1",
  hostId,
  blockedReason = null,
  onSent,
}: {
  sessionId?: string;
  hostId?: string;
  blockedReason?: string | null;
  onSent?: () => void;
}) {
  const { dragging, status, handlers } = useTerminalFileDrop({
    sessionId,
    hostId,
    blockedReason,
    onSent,
  });
  return (
    <div data-testid="target" data-dragging={dragging} {...handlers}>
      <span data-testid="child" />
      <span data-testid="status">{status ?? "idle"}</span>
    </div>
  );
}

type FakeFile = File & { readBuffer: ReturnType<typeof vi.fn> };

function makeFile(name: string, bytes: number[], size?: number): FakeFile {
  const file = new File([new Uint8Array(bytes)], name) as FakeFile;
  const readBuffer = vi.fn(async () => new Uint8Array(bytes).buffer);
  Object.defineProperty(file, "arrayBuffer", { value: readBuffer });
  if (size !== undefined) Object.defineProperty(file, "size", { value: size });
  file.readBuffer = readBuffer;
  return file;
}

function transfer(
  files: File[],
  extra: Partial<{ items: unknown[]; types: string[] }> = {},
) {
  return {
    types: extra.types ?? ["Files"],
    files,
    items: extra.items ?? files.map(() => ({ kind: "file" })),
    dropEffect: "none",
  };
}

const drop = (el: Element, dt: ReturnType<typeof transfer>) =>
  fireEvent.drop(el, { dataTransfer: dt });

/** The files handed to the host, as names and plain byte arrays. */
const sentFiles = (call = 0) =>
  (
    mockSendFiles.mock.calls[call][1] as { name: string; bytes: Uint8Array }[]
  ).map((f) => ({ name: f.name, bytes: [...f.bytes] }));
const sentSignal = (call = 0) =>
  (mockSendFiles.mock.calls[call][2] as { signal: AbortSignal }).signal;

beforeEach(() => {
  vi.clearAllMocks();
  mockSendFiles.mockResolvedValue(undefined);
  // Nothing about a drop may reach the server.
  vi.stubGlobal("fetch", mockFetch);
});

describe("useTerminalFileDrop", () => {
  it("sends every dropped file in order with exact bytes and original names", async () => {
    const onSent = vi.fn();
    renderWithProviders(<Harness onSent={onSent} />);
    const files = [
      makeFile("my notes (1).txt", [104, 105]),
      makeFile("café ✓.bin", [0, 255, 128]),
      makeFile("c.md", [1]),
    ];

    drop(screen.getByTestId("target"), transfer(files));

    await waitFor(() => expect(onSent).toHaveBeenCalledTimes(1));
    expect(mockSendFiles).toHaveBeenCalledTimes(1);
    expect(mockSendFiles.mock.calls[0][0]).toBe("sess-1");
    expect(sentFiles()).toEqual([
      { name: "my notes (1).txt", bytes: [104, 105] },
      { name: "café ✓.bin", bytes: [0, 255, 128] },
      { name: "c.md", bytes: [1] },
    ]);
    expect(sentSignal()).toBeInstanceOf(AbortSignal);
    expect(mockFetch).not.toHaveBeenCalled();
    expect(screen.getByTestId("status")).toHaveTextContent("idle");
  });

  it("names the host it was given, and the active host otherwise", async () => {
    const onSent = vi.fn();
    const { unmount } = renderWithProviders(<Harness onSent={onSent} />);
    drop(screen.getByTestId("target"), transfer([makeFile("a.bin", [1])]));
    await waitFor(() => expect(onSent).toHaveBeenCalled());
    expect(mockSendFiles.mock.calls[0][3]).toBeUndefined();
    unmount();
    renderWithProviders(<Harness hostId="host-b" onSent={onSent} />);
    drop(screen.getByTestId("target"), transfer([makeFile("b.bin", [2])]));
    await waitFor(() => expect(onSent).toHaveBeenCalledTimes(2));
    expect(mockSendFiles.mock.calls[1][3]).toBe("host-b");
  });

  it("shows the host's refusal and sends nothing else", async () => {
    mockSendFiles.mockRejectedValue(
      new HostControlError(
        "The host refused this transfer. Nothing was pasted.",
        "refused",
      ),
    );
    renderWithProviders(<Harness />);
    drop(screen.getByTestId("target"), transfer([makeFile("a.bin", [1])]));
    expect(await screen.findByText(/host refused this transfer/)).toBeVisible();
    expect(mockSendFiles).toHaveBeenCalledTimes(1);
    expect(mockFetch).not.toHaveBeenCalled();
  });

  it("rejects a too-large drop before reading any file", () => {
    renderWithProviders(<Harness />);
    const files = [
      makeFile("a.bin", [1], TERMINAL_DROP_MAX_BYTES / 2 + 1),
      makeFile("b.bin", [1], TERMINAL_DROP_MAX_BYTES / 2),
    ];

    drop(screen.getByTestId("target"), transfer(files));

    files.forEach((f) => expect(f.readBuffer).not.toHaveBeenCalled());
    expect(mockSendFiles).not.toHaveBeenCalled();
    expect(screen.getByText(/more than 9 MiB.*fewer or smaller/)).toBeVisible();
  });

  it("rejects too many files before reading any file", () => {
    renderWithProviders(<Harness />);
    const files = Array.from({ length: TERMINAL_DROP_MAX_FILES + 1 }, (_, i) =>
      makeFile(`f${i}.txt`, [i]),
    );

    drop(screen.getByTestId("target"), transfer(files));

    files.forEach((f) => expect(f.readBuffer).not.toHaveBeenCalled());
    expect(mockSendFiles).not.toHaveBeenCalled();
    expect(screen.getByText(/at most 16 files.*Drop fewer/)).toBeVisible();
  });

  it("accepts exactly the file-count and size limits", async () => {
    const onSent = vi.fn();
    renderWithProviders(<Harness onSent={onSent} />);
    const files = Array.from({ length: TERMINAL_DROP_MAX_FILES }, (_, i) =>
      makeFile(
        `f${i}.txt`,
        [i],
        TERMINAL_DROP_MAX_BYTES / TERMINAL_DROP_MAX_FILES,
      ),
    );

    drop(screen.getByTestId("target"), transfer(files));

    await waitFor(() => expect(onSent).toHaveBeenCalled());
    expect(mockSendFiles.mock.calls[0][1]).toHaveLength(16);
  });

  it("rejects a dropped folder explicitly and sends nothing", () => {
    renderWithProviders(<Harness />);
    const file = makeFile("src", []);
    const dt = transfer([file], {
      items: [
        { kind: "file", webkitGetAsEntry: () => ({ isDirectory: true }) },
      ],
    });

    drop(screen.getByTestId("target"), dt);

    expect(file.readBuffer).not.toHaveBeenCalled();
    expect(mockSendFiles).not.toHaveBeenCalled();
    expect(screen.getByText(/Folders can't be attached/)).toBeVisible();
  });

  it("explains why a blocked drop was refused, never reads, and still stops navigation", () => {
    renderWithProviders(
      <Harness blockedReason="You have read-only access to this session." />,
    );
    const file = makeFile("a.txt", [1]);

    const notCancelled = drop(screen.getByTestId("target"), transfer([file]));

    expect(notCancelled).toBe(false);
    expect(file.readBuffer).not.toHaveBeenCalled();
    expect(mockSendFiles).not.toHaveBeenCalled();
    expect(
      screen.getByText("You have read-only access to this session."),
    ).toBeVisible();
  });

  it("marks a drag over a blocked terminal as not droppable", () => {
    renderWithProviders(<Harness blockedReason="No CLI host is attached." />);
    const dt = transfer([]);

    const notCancelled = fireEvent.dragOver(screen.getByTestId("target"), {
      dataTransfer: dt,
    });

    expect(notCancelled).toBe(false);
    expect(dt.dropEffect).toBe("none");
  });

  it("re-checks readiness after the files are read", async () => {
    let release!: () => void;
    const file = makeFile("a.txt", [1]);
    file.readBuffer.mockImplementation(
      () =>
        new Promise<ArrayBuffer>((resolve) => {
          release = () => resolve(new Uint8Array([1]).buffer);
        }),
    );
    const { rerender } = renderWithProviders(<Harness />);

    drop(screen.getByTestId("target"), transfer([file]));
    await waitFor(() =>
      expect(screen.getByTestId("status")).toHaveTextContent("reading"),
    );
    rerender(<Harness blockedReason="The terminal isn't live yet." />);
    await act(async () => release());

    expect(mockSendFiles).not.toHaveBeenCalled();
    expect(
      await screen.findByText("The terminal isn't live yet."),
    ).toBeVisible();
    expect(screen.getByTestId("status")).toHaveTextContent("idle");
  });

  it("does not send an old session's files or call the new onSent when its read resolves after a switch", async () => {
    let release!: () => void;
    const file = makeFile("a.txt", [1]);
    file.readBuffer.mockImplementation(
      () =>
        new Promise<ArrayBuffer>((resolve) => {
          release = () => resolve(new Uint8Array([1]).buffer);
        }),
    );
    const oldOnSent = vi.fn();
    const newOnSent = vi.fn();
    const { rerender } = renderWithProviders(<Harness onSent={oldOnSent} />);

    drop(screen.getByTestId("target"), transfer([file]));
    await waitFor(() =>
      expect(screen.getByTestId("status")).toHaveTextContent("reading"),
    );
    rerender(<Harness sessionId="sess-2" onSent={newOnSent} />);
    await act(async () => release());

    expect(mockSendFiles).not.toHaveBeenCalled();
    expect(oldOnSent).not.toHaveBeenCalled();
    expect(newOnSent).not.toHaveBeenCalled();
  });

  it("ignores a second drop while one is in flight", async () => {
    let finish!: () => void;
    mockSendFiles.mockReturnValue(
      new Promise((resolve) => {
        finish = () => resolve(undefined);
      }),
    );
    const onSent = vi.fn();
    renderWithProviders(<Harness onSent={onSent} />);
    const target = screen.getByTestId("target");

    drop(target, transfer([makeFile("a.txt", [1])]));
    await waitFor(() =>
      expect(screen.getByTestId("status")).toHaveTextContent("sending"),
    );
    drop(target, transfer([makeFile("b.txt", [2])]));

    expect(await screen.findByText(/still being sent/)).toBeVisible();
    expect(mockSendFiles).toHaveBeenCalledTimes(1);
    await act(async () => finish());
    expect(onSent).toHaveBeenCalledTimes(1);
  });

  it("shows the host's deliberate message and does not retry", async () => {
    mockSendFiles.mockRejectedValue(
      new HostControlError(
        "The terminal host is busy. Try again in a moment.",
        "failed",
      ),
    );
    const onSent = vi.fn();
    renderWithProviders(<Harness onSent={onSent} />);

    drop(screen.getByTestId("target"), transfer([makeFile("a.txt", [1])]));

    expect(
      await screen.findByText(
        "The terminal host is busy. Try again in a moment.",
      ),
    ).toBeVisible();
    expect(mockSendFiles).toHaveBeenCalledTimes(1);
    expect(onSent).not.toHaveBeenCalled();
    expect(screen.getByTestId("status")).toHaveTextContent("idle");
  });

  it("replaces a raw transport failure with a next-step message", async () => {
    mockSendFiles.mockRejectedValue(
      new HostControlError("Failed to fetch", "transport"),
    );
    renderWithProviders(<Harness />);

    drop(screen.getByTestId("target"), transfer([makeFile("a.txt", [1])]));

    expect(
      await screen.findByText(/may not have been delivered.*Check the prompt/),
    ).toBeVisible();
    expect(screen.queryByText(/Failed to fetch/)).toBeNull();
    expect(mockSendFiles).toHaveBeenCalledTimes(1);
  });

  it("explains an unreadable file instead of leaking the exception", async () => {
    const file = makeFile("gone.txt", [1]);
    file.readBuffer.mockRejectedValue(new DOMException("NotReadableError"));
    renderWithProviders(<Harness />);

    drop(screen.getByTestId("target"), transfer([file]));

    expect(
      await screen.findByText(/Could not read the dropped files/),
    ).toBeVisible();
    expect(screen.queryByText(/NotReadableError/)).toBeNull();
    expect(mockSendFiles).not.toHaveBeenCalled();
  });

  it("drops a late success after the session switches", async () => {
    let finish!: () => void;
    mockSendFiles.mockReturnValue(
      new Promise((resolve) => {
        finish = () => resolve(undefined);
      }),
    );
    const onSent = vi.fn();
    const { rerender } = renderWithProviders(<Harness onSent={onSent} />);

    drop(screen.getByTestId("target"), transfer([makeFile("a.txt", [1])]));
    await waitFor(() => expect(mockSendFiles).toHaveBeenCalled());
    const signal = sentSignal();
    rerender(<Harness sessionId="sess-2" onSent={onSent} />);
    await act(async () => finish());

    expect(signal.aborted).toBe(true);
    expect(onSent).not.toHaveBeenCalled();
    expect(screen.getByTestId("status")).toHaveTextContent("idle");
  });

  it("stays silent when a late failure arrives after the session switches", async () => {
    let fail!: () => void;
    mockSendFiles.mockReturnValue(
      new Promise((_, reject) => {
        fail = () => reject(new HostControlError("Host exploded.", "failed"));
      }),
    );
    const { rerender } = renderWithProviders(<Harness />);

    drop(screen.getByTestId("target"), transfer([makeFile("a.txt", [1])]));
    await waitFor(() => expect(mockSendFiles).toHaveBeenCalled());
    rerender(<Harness sessionId="sess-2" />);
    await act(async () => fail());

    expect(screen.queryByText("Host exploded.")).toBeNull();
  });

  it("aborts the request on unmount and ignores its completion", async () => {
    let finish!: () => void;
    mockSendFiles.mockReturnValue(
      new Promise((resolve) => {
        finish = () => resolve(undefined);
      }),
    );
    const onSent = vi.fn();
    const { unmount } = renderWithProviders(<Harness onSent={onSent} />);

    drop(screen.getByTestId("target"), transfer([makeFile("a.txt", [1])]));
    await waitFor(() => expect(mockSendFiles).toHaveBeenCalled());
    const signal = sentSignal();
    unmount();
    await act(async () => finish());

    expect(signal.aborted).toBe(true);
    expect(onSent).not.toHaveBeenCalled();
  });

  it("allows a new drop on the next session while the old one is still pending", async () => {
    mockSendFiles.mockReturnValueOnce(new Promise(() => {}));
    const onSent = vi.fn();
    const { rerender } = renderWithProviders(<Harness onSent={onSent} />);

    drop(screen.getByTestId("target"), transfer([makeFile("a.txt", [1])]));
    await waitFor(() => expect(mockSendFiles).toHaveBeenCalledTimes(1));
    rerender(<Harness sessionId="sess-2" onSent={onSent} />);
    drop(screen.getByTestId("target"), transfer([makeFile("b.txt", [2])]));

    await waitFor(() => expect(onSent).toHaveBeenCalledTimes(1));
    expect(mockSendFiles.mock.calls[1][0]).toBe("sess-2");
  });

  it("keeps the drag highlight while moving across nested elements", () => {
    renderWithProviders(<Harness />);
    const target = screen.getByTestId("target");
    const child = screen.getByTestId("child");
    const dt = transfer([]);

    fireEvent.dragEnter(target, { dataTransfer: dt });
    fireEvent.dragEnter(child, { dataTransfer: dt });
    fireEvent.dragLeave(target, { dataTransfer: dt });
    expect(target).toHaveAttribute("data-dragging", "true");

    fireEvent.dragLeave(child, { dataTransfer: dt });
    expect(target).toHaveAttribute("data-dragging", "false");
  });

  it("clears the highlight on drop", () => {
    renderWithProviders(<Harness />);
    const target = screen.getByTestId("target");
    const dt = transfer([]);

    fireEvent.dragEnter(target, { dataTransfer: dt });
    drop(target, dt);

    expect(target).toHaveAttribute("data-dragging", "false");
  });

  it("ignores non-file drags such as dragged text", () => {
    renderWithProviders(<Harness />);

    const notCancelled = drop(
      screen.getByTestId("target"),
      transfer([], { types: ["text/plain"] }),
    );

    expect(notCancelled).toBe(true);
    expect(mockSendFiles).not.toHaveBeenCalled();
  });
});
