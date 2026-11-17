import * as React from "react";
import { mermaidDimensions, mermaidFrameDocument } from "@/lib/mermaid";
import { CodeCopyButton } from "./CodeBlock";
import {
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog";

export function MermaidViewport({
  svg,
  source,
}: {
  svg: string;
  source: string;
}) {
  const viewport = React.useRef<HTMLDivElement>(null);
  const { width, height } = React.useMemo(() => mermaidDimensions(svg), [svg]);
  const frame = React.useMemo(() => mermaidFrameDocument(svg, true), [svg]);
  const [scale, setScale] = React.useState(1);
  const fit = React.useCallback(() => {
    const node = viewport.current;
    if (!node || !node.clientWidth || !node.clientHeight) return;
    setScale(
      Math.max(
        0.01,
        Math.min(
          1,
          (node.clientWidth - 16) / width,
          (node.clientHeight - 16) / height,
        ),
      ),
    );
    node.scrollTo?.(0, 0);
  }, [width, height]);
  const attachViewport = React.useCallback(
    (node: HTMLDivElement | null) => {
      viewport.current = node;
      if (node) fit();
    },
    [fit],
  );
  React.useLayoutEffect(() => {
    fit();
    window.addEventListener("resize", fit);
    return () => window.removeEventListener("resize", fit);
  }, [fit]);

  const buttonClass =
    "rounded border border-border px-2 py-1 text-xs hover:bg-accent focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring";
  return (
    <>
      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          aria-label="Zoom out"
          className={buttonClass}
          onClick={() => setScale((value) => Math.max(0.01, value / 1.25))}
        >
          −
        </button>
        <output
          aria-label="Diagram zoom"
          className="min-w-12 text-center text-xs tabular-nums"
        >
          {Math.round(scale * 100)}%
        </output>
        <button
          type="button"
          aria-label="Zoom in"
          className={buttonClass}
          onClick={() => setScale((value) => Math.min(8, value * 1.25))}
        >
          +
        </button>
        <button type="button" className={buttonClass} onClick={fit}>
          Fit
        </button>
        <button
          type="button"
          aria-label="Actual size"
          className={buttonClass}
          onClick={() => setScale(1)}
        >
          1:1
        </button>
        <CodeCopyButton code={source} label="Copy Mermaid source" />
      </div>
      <div
        ref={attachViewport}
        className="min-h-0 flex-1 overflow-auto rounded-md bg-white p-2"
        role="region"
        tabIndex={0}
        aria-label="Scrollable diagram"
      >
        <iframe
          tabIndex={-1}
          title="Expanded Mermaid diagram"
          sandbox=""
          referrerPolicy="no-referrer"
          srcDoc={frame}
          className="pointer-events-none mx-auto block max-w-none border-0"
          style={{ width: width * scale, height: height * scale }}
        />
      </div>
    </>
  );
}

export function MermaidPopup({ svg, source }: { svg: string; source: string }) {
  return (
    <DialogContent className="flex h-[90dvh] w-[calc(100vw-2rem)] max-w-none flex-col p-4">
      <DialogHeader className="mb-0 pr-8">
        <DialogTitle>Mermaid diagram</DialogTitle>
        <DialogDescription>
          Zoom in to read labels. Scroll to explore the whole diagram.
        </DialogDescription>
      </DialogHeader>
      <MermaidViewport svg={svg} source={source} />
    </DialogContent>
  );
}
