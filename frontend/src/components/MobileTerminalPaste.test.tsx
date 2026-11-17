import { afterEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { MobileTerminalPaste } from "./MobileTerminalPaste";
import { Terminal } from "@xterm/xterm";

afterEach(() => vi.unstubAllGlobals());

describe("mobile terminal paste", () => {
  it("uses real xterm bracketed paste without appending Enter", async () => {
    vi.stubGlobal("navigator", {
      clipboard: { readText: vi.fn().mockResolvedValue("hello\n世界") },
    });
    const term = new Terminal();
    // No renderer in jsdom: provide the helper textarea that xterm clears after
    // emitting paste data, while keeping the actual parser/paste implementation.
    (
      term as unknown as { _core: { textarea: HTMLTextAreaElement } }
    )._core.textarea = document.createElement("textarea");
    const input = vi.fn();
    term.onData(input);
    try {
      await new Promise<void>((done) => term.write("\x1b[?2004h", done));
      render(
        <MobileTerminalPaste
          onImage={vi.fn()}
          onPaste={(text) => term.paste(text)}
        />,
      );
      await act(async () =>
        fireEvent.click(screen.getByRole("button", { name: "Paste" })),
      );
      expect(input).toHaveBeenCalledExactlyOnceWith(
        "\x1b[200~hello\r世界\x1b[201~",
      );
      expect(screen.queryByRole("textbox")).not.toBeInTheDocument();
    } finally {
      term.dispose();
    }
  });

  it("reads clipboard only after a tap and passes multiline Unicode unchanged", async () => {
    const readText = vi.fn().mockResolvedValue("hello\n世界\n");
    vi.stubGlobal("navigator", { clipboard: { readText } });
    const paste = vi.fn();
    render(<MobileTerminalPaste onImage={vi.fn()} onPaste={paste} />);
    expect(readText).not.toHaveBeenCalled();
    await act(async () =>
      fireEvent.click(screen.getByRole("button", { name: "Paste" })),
    );
    expect(readText).toHaveBeenCalledTimes(1);
    expect(paste).toHaveBeenCalledExactlyOnceWith("hello\n世界\n");
  });

  it.each(["missing", "denied", "empty"])(
    "offers native paste when clipboard is %s",
    async (mode) => {
      vi.stubGlobal(
        "navigator",
        mode === "missing"
          ? {}
          : {
              clipboard: {
                readText:
                  mode === "denied"
                    ? vi.fn().mockRejectedValue(new Error("denied"))
                    : vi.fn().mockResolvedValue(""),
              },
            },
      );
      const paste = vi.fn();
      render(<MobileTerminalPaste onImage={vi.fn()} onPaste={paste} />);
      await act(async () =>
        fireEvent.click(screen.getByRole("button", { name: "Paste" })),
      );
      expect(paste).not.toHaveBeenCalled();
      fireEvent.change(screen.getByRole("textbox"), {
        target: { value: "  text\nsecond line" },
      });
      fireEvent.click(
        screen.getByRole("button", { name: "Paste into terminal" }),
      );
      expect(paste).toHaveBeenCalledExactlyOnceWith("  text\nsecond line");
      expect(screen.queryByRole("textbox")).not.toBeInTheDocument();
    },
  );

  it("pastes text from the rich clipboard API when no image is present", async () => {
    const text = "hello\n世界";
    vi.stubGlobal("navigator", {
      clipboard: {
        read: vi.fn().mockResolvedValue([
          {
            types: ["text/plain"],
            getType: async () => ({ text: async () => text }),
          },
        ]),
      },
    });
    const onPaste = vi.fn();
    const onImage = vi.fn();
    render(<MobileTerminalPaste onPaste={onPaste} onImage={onImage} />);
    await act(async () =>
      fireEvent.click(screen.getByRole("button", { name: "Paste" })),
    );
    expect(onPaste).toHaveBeenCalledExactlyOnceWith(text);
    expect(onImage).not.toHaveBeenCalled();
  });

  it("prefers a clipboard screenshot over its text representation", async () => {
    const image = new Blob(["screenshot"], { type: "image/png" });
    const getType = vi.fn().mockResolvedValue(image);
    const read = vi
      .fn()
      .mockResolvedValue([{ types: ["text/plain", "image/png"], getType }]);
    vi.stubGlobal("navigator", { clipboard: { read } });
    const onImage = vi.fn().mockResolvedValue(true);
    const onPaste = vi.fn();
    render(<MobileTerminalPaste onPaste={onPaste} onImage={onImage} />);
    expect(read).not.toHaveBeenCalled();
    await act(async () =>
      fireEvent.click(screen.getByRole("button", { name: "Paste" })),
    );
    expect(getType).toHaveBeenCalledExactlyOnceWith("image/png");
    expect(onImage).toHaveBeenCalledExactlyOnceWith(image, "image/png");
    expect(onPaste).not.toHaveBeenCalled();
    expect(screen.queryByRole("textbox")).not.toBeInTheDocument();
  });

  it("accepts a screenshot through native paste after clipboard access is denied", async () => {
    vi.stubGlobal("navigator", {
      clipboard: { read: vi.fn().mockRejectedValue(new Error("denied")) },
    });
    const onImage = vi.fn().mockResolvedValue(true);
    const onPaste = vi.fn();
    render(<MobileTerminalPaste onPaste={onPaste} onImage={onImage} />);
    await act(async () =>
      fireEvent.click(screen.getByRole("button", { name: "Paste" })),
    );
    const image = new File(["screenshot"], "screen.png", { type: "image/png" });
    await act(async () =>
      fireEvent.paste(screen.getByRole("textbox"), {
        clipboardData: {
          items: [{ type: "image/png", getAsFile: () => image }],
        },
      }),
    );
    expect(onImage).toHaveBeenCalledExactlyOnceWith(image, "image/png");
    expect(onPaste).not.toHaveBeenCalled();
    expect(screen.queryByRole("textbox")).not.toBeInTheDocument();
  });

  it.each(["rejected", "no-text"])(
    "tries text-only paste when rich clipboard is %s",
    async (mode) => {
      const readText = vi.fn().mockResolvedValue("plain fallback");
      vi.stubGlobal("navigator", {
        clipboard: {
          read:
            mode === "rejected"
              ? vi.fn().mockRejectedValue(new Error("denied"))
              : vi.fn().mockResolvedValue([]),
          readText,
        },
      });
      const onPaste = vi.fn();
      render(<MobileTerminalPaste onImage={vi.fn()} onPaste={onPaste} />);
      await act(async () =>
        fireEvent.click(screen.getByRole("button", { name: "Paste" })),
      );
      expect(readText).toHaveBeenCalledTimes(1);
      expect(onPaste).toHaveBeenCalledExactlyOnceWith("plain fallback");
      expect(screen.queryByRole("textbox")).not.toBeInTheDocument();
    },
  );

  it("offers manual paste during a pending read and never delivers its late image", async () => {
    let resolve!: (items: unknown[]) => void;
    vi.stubGlobal("navigator", {
      clipboard: {
        read: () =>
          new Promise((done) => {
            resolve = done;
          }),
      },
    });
    const onImage = vi.fn();
    const onPaste = vi.fn();
    render(<MobileTerminalPaste onImage={onImage} onPaste={onPaste} />);
    fireEvent.click(screen.getByRole("button", { name: "Paste" }));
    expect(screen.getByRole("button", { name: "Pasting…" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Paste manually" }));
    fireEvent.change(screen.getByRole("textbox"), {
      target: { value: "keep this draft" },
    });
    const getType = vi
      .fn()
      .mockResolvedValue(new Blob(["late image"], { type: "image/png" }));
    await act(async () => resolve([{ types: ["image/png"], getType }]));
    expect(onImage).not.toHaveBeenCalled();
    expect(onPaste).not.toHaveBeenCalled();
    expect(getType).not.toHaveBeenCalled();
    expect(screen.getByRole("textbox")).toHaveValue("keep this draft");
    expect(screen.getByRole("button", { name: "Paste" })).toBeEnabled();
  });

  it.each(["native", "button"])(
    "keeps the draft after failed image delivery through %s paste",
    async (source) => {
      const image = new File(["screenshot"], "screen.png", {
        type: "image/png",
      });
      const read = vi.fn().mockRejectedValueOnce(new Error("denied"));
      vi.stubGlobal("navigator", { clipboard: { read } });
      const onImage = vi.fn().mockResolvedValue(false);
      const onPaste = vi.fn();
      render(<MobileTerminalPaste onImage={onImage} onPaste={onPaste} />);
      await act(async () =>
        fireEvent.click(screen.getByRole("button", { name: "Paste" })),
      );
      fireEvent.change(screen.getByRole("textbox"), {
        target: { value: "keep this draft" },
      });
      if (source === "native") {
        await act(async () =>
          fireEvent.paste(screen.getByRole("textbox"), {
            clipboardData: {
              items: [{ type: "image/png", getAsFile: () => image }],
            },
          }),
        );
      } else {
        read.mockResolvedValueOnce([
          { types: ["image/png"], getType: async () => image },
        ]);
        await act(async () =>
          fireEvent.click(screen.getByRole("button", { name: "Paste" })),
        );
      }
      expect(onImage).toHaveBeenCalledExactlyOnceWith(image, "image/png");
      expect(screen.getByRole("textbox")).toHaveValue("keep this draft");
      expect(onPaste).not.toHaveBeenCalled();
    },
  );

  it("handles rejected native image delivery without losing the draft", async () => {
    vi.stubGlobal("navigator", {});
    const onImage = vi.fn().mockRejectedValue(new Error("host refused"));
    render(<MobileTerminalPaste onImage={onImage} onPaste={vi.fn()} />);
    await act(async () =>
      fireEvent.click(screen.getByRole("button", { name: "Paste" })),
    );
    fireEvent.change(screen.getByRole("textbox"), {
      target: { value: "keep this draft" },
    });
    await act(async () =>
      fireEvent.paste(screen.getByRole("textbox"), {
        clipboardData: {
          items: [
            {
              type: "image/png",
              getAsFile: () => new File(["image"], "screen.png"),
            },
          ],
        },
      }),
    );
    expect(screen.getByRole("textbox")).toHaveValue("keep this draft");
  });

  it("has no separate Type text button", () => {
    render(<MobileTerminalPaste onImage={vi.fn()} onPaste={vi.fn()} />);
    expect(
      screen.queryByRole("button", { name: "Type text" }),
    ).not.toBeInTheDocument();
  });

  it("discards a manual draft on Cancel", async () => {
    vi.stubGlobal("navigator", {});
    const paste = vi.fn();
    render(<MobileTerminalPaste onImage={vi.fn()} onPaste={paste} />);
    await act(async () =>
      fireEvent.click(screen.getByRole("button", { name: "Paste" })),
    );
    fireEvent.change(screen.getByRole("textbox"), {
      target: { value: "draft" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(paste).not.toHaveBeenCalled();
    expect(screen.queryByRole("textbox")).not.toBeInTheDocument();
  });

  it("ignores a clipboard permission response after leaving the terminal", async () => {
    let resolve!: (text: string) => void;
    vi.stubGlobal("navigator", {
      clipboard: {
        readText: () =>
          new Promise<string>((done) => {
            resolve = done;
          }),
      },
    });
    const paste = vi.fn();
    const { unmount } = render(
      <MobileTerminalPaste onImage={vi.fn()} onPaste={paste} />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Paste" }));
    unmount();
    await act(async () => resolve("stale clipboard"));
    expect(paste).not.toHaveBeenCalled();
  });
});
