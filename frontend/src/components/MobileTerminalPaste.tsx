import * as React from "react";

/** A user-initiated clipboard read, with a native paste field for mobile browsers. */
export function MobileTerminalPaste({
  onPaste,
  onImage,
}: {
  onPaste: (text: string) => void;
  onImage: (image: Blob, mime: string) => Promise<boolean>;
}) {
  const [reading, setReading] = React.useState(false);
  const [waitingForClipboard, setWaitingForClipboard] = React.useState(false);
  const attempt = React.useRef(0);
  const [manual, setManual] = React.useState(false);
  const [text, setText] = React.useState("");
  const inputId = React.useId();
  const mounted = React.useRef(false);
  React.useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  async function readClipboard() {
    const request = ++attempt.current;
    const current = () => mounted.current && attempt.current === request;
    setReading(true);
    setWaitingForClipboard(true);
    try {
      let content: { image: Blob; mime: string } | { text: string } | null =
        null;
      if (navigator.clipboard?.read) {
        try {
          const items = await navigator.clipboard.read();
          if (!current()) return;
          for (const item of items) {
            const mime = item.types.find((type) => type.startsWith("image/"));
            if (!mime) continue;
            content = { image: await item.getType(mime), mime };
            break;
          }
          if (!content) {
            const item = items.find((item) =>
              item.types.includes("text/plain"),
            );
            if (item) {
              const value = await (await item.getType("text/plain")).text();
              if (value) content = { text: value };
            }
          }
        } catch {
          // A text-only read may still work when the rich clipboard read fails.
        }
      }
      if (!current()) return;
      if (!content && navigator.clipboard?.readText) {
        content = { text: await navigator.clipboard.readText() };
      }
      if (!current()) return;
      setWaitingForClipboard(false);
      if (content && "image" in content) {
        const delivered = await onImage(content.image, content.mime);
        if (current() && delivered) {
          setText("");
          setManual(false);
        }
      } else if (content?.text) {
        onPaste(content.text);
        setText("");
        setManual(false);
      } else setManual(true);
    } catch {
      if (current()) setManual(true);
    } finally {
      if (current()) {
        setReading(false);
        setWaitingForClipboard(false);
      }
    }
  }

  async function pasteNativeImage(image: Blob, mime: string) {
    try {
      const delivered = await onImage(image, mime);
      if (mounted.current && delivered) {
        setText("");
        setManual(false);
      }
    } catch {
      // Keep the draft and native paste field when delivery fails.
    }
  }

  return (
    <>
      <button
        type="button"
        disabled={reading}
        onPointerDown={(event) => event.preventDefault()}
        onClick={() => void readClipboard()}
        className="min-w-10 rounded-md border border-border bg-card px-2.5 py-2 text-sm font-medium text-foreground transition-colors active:bg-emerald-500/15 disabled:opacity-50"
      >
        {reading ? "Pasting…" : "Paste"}
      </button>
      {waitingForClipboard && (
        <button
          type="button"
          onClick={() => {
            // The browser read cannot be aborted. Ignore its eventual result.
            attempt.current++;
            setReading(false);
            setWaitingForClipboard(false);
            setManual(true);
          }}
          className="rounded-md border border-border px-2.5 py-2 text-sm"
        >
          Paste manually
        </button>
      )}
      {manual && (
        <form
          data-terminal-paste
          className="w-full space-y-2"
          onSubmit={(event) => {
            event.preventDefault();
            if (!text) return;
            onPaste(text);
            setText("");
            setManual(false);
          }}
        >
          <label className="block text-sm" htmlFor={inputId}>
            Use your phone’s Paste action below for text or a screenshot. Use
            Enter on the terminal toolbar to submit text.
          </label>
          <textarea
            id={inputId}
            aria-label="Text to paste into terminal"
            onPaste={(event) => {
              const items = event.clipboardData.items;
              for (let i = 0; i < items.length; i++) {
                if (!items[i].type.startsWith("image/")) continue;
                const image = items[i].getAsFile();
                if (!image) continue;
                event.preventDefault();
                void pasteNativeImage(image, items[i].type);
                return;
              }
            }}
            value={text}
            onChange={(event) => setText(event.target.value)}
            rows={3}
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            className="w-full rounded-md border border-border bg-background p-2 text-base"
          />
          <div className="flex gap-2">
            <button
              type="submit"
              disabled={!text}
              className="rounded-md border border-border px-3 py-2 text-sm disabled:opacity-50"
            >
              Paste into terminal
            </button>
            <button
              type="button"
              onClick={() => {
                setText("");
                setManual(false);
              }}
              className="rounded-md border border-border px-3 py-2 text-sm"
            >
              Cancel
            </button>
          </div>
        </form>
      )}
    </>
  );
}
