import * as React from "react";
import { CodeBlock, CodeCopyButton } from "./CodeBlock";

export function MarkdownBlock({
  source,
  preview,
}: {
  source: string;
  preview: React.ReactNode;
}) {
  const [mode, setMode] = React.useState<"code" | "preview">("code");

  // Chat text can grow while streaming. Return to source when it changes so a
  // preview never implies that a partial definition is the final document.
  React.useLayoutEffect(() => setMode("code"), [source]);

  if (mode === "code") {
    return (
      <CodeBlock
        code={source}
        language="markdown"
        actions={
          <button
            type="button"
            onClick={() => setMode("preview")}
            className="rounded px-2 py-0.5 text-[0.7rem] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
            aria-label="View rendered Markdown"
          >
            View
          </button>
        }
      />
    );
  }

  return (
    <div className="group/markdown my-[0.8em] overflow-hidden rounded-(--radius) border border-border bg-card">
      <div className="flex items-center justify-between border-b border-border bg-muted/60 px-3 py-1.5">
        <span className="text-[0.7rem] font-medium uppercase tracking-wide text-muted-foreground">
          Markdown preview
        </span>
        <div className="flex items-center gap-1 transition-opacity sm:opacity-0 sm:group-hover/markdown:opacity-100 sm:group-focus-within/markdown:opacity-100">
          <button
            type="button"
            onClick={() => setMode("code")}
            className="rounded px-2 py-0.5 text-[0.7rem] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
            aria-label="View Markdown source"
          >
            Code
          </button>
          <CodeCopyButton code={source} label="Copy Markdown source" />
        </div>
      </div>
      <div className="p-4" data-testid="markdown-preview">
        {preview}
      </div>
    </div>
  );
}
