import * as React from "react";
import { highlightCode } from "@/lib/highlight";

interface CodeBlockProps {
  /** raw text content of the code block */
  code: string;
  /** language label (e.g. "ts", "json") */
  language?: string;
  /** pre-highlighted children (from react-markdown rehype-highlight) */
  children?: React.ReactNode;
  /** extra controls shown before Copy in the code-block toolbar */
  actions?: React.ReactNode;
}

export function CodeCopyButton({
  code,
  label = "Copy code",
}: {
  code: string;
  label?: string;
}) {
  const [copied, setCopied] = React.useState(false);

  const onCopy = React.useCallback(() => {
    navigator.clipboard
      ?.writeText(code)
      .then(() => {
        setCopied(true);
        window.setTimeout(() => setCopied(false), 1500);
      })
      .catch(() => undefined);
  }, [code]);

  return (
    <button
      type="button"
      onClick={onCopy}
      className={
        copied
          ? "flex items-center gap-1 rounded px-2 py-0.5 text-[0.7rem] font-medium text-green-400 transition-colors"
          : "flex items-center gap-1 rounded px-2 py-0.5 text-[0.7rem] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
      }
      aria-label={copied ? "Copied" : label}
    >
      {copied ? (
        <>
          <svg
            aria-hidden="true"
            viewBox="0 0 16 16"
            className="h-3 w-3"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <path d="M13.5 4.5 6 12 2.5 8.5" />
          </svg>
          Copied
        </>
      ) : (
        "Copy"
      )}
    </button>
  );
}

/**
 * A styled code-block container with a language label and a copy button.
 * When `children` is provided (the already-highlighted <code> from
 * react-markdown) it is rendered as-is. Otherwise we highlight `code` with
 * highlight.js directly so this path matches the Markdown/rehype path.
 */
export function CodeBlock({
  code,
  language,
  children,
  actions,
}: CodeBlockProps) {
  // No pre-highlighted children: highlight `code` ourselves. hljs escapes its
  // own output, so the resulting HTML is safe to inject. If the language isn't
  // registered, fall back to plain text under the `.hljs` base class (still
  // gets the theme's background/foreground), never unstyled.
  const highlighted = React.useMemo(
    () => (children == null && language ? highlightCode(code, language) : null),
    [children, code, language],
  );

  return (
    <div className="code-block group">
      <div className="flex items-center justify-between border-b border-border bg-black/30 px-3 py-1.5">
        <span className="text-[0.7rem] font-medium uppercase tracking-wide text-muted-foreground">
          {language || "code"}
        </span>
        <div className="flex items-center gap-1">
          {actions}
          <CodeCopyButton code={code} />
        </div>
      </div>
      <pre>
        {children ??
          (highlighted != null ? (
            <code
              className={`hljs language-${language}`}
              dangerouslySetInnerHTML={{ __html: highlighted }}
            />
          ) : (
            <code className="hljs">{code}</code>
          ))}
      </pre>
    </div>
  );
}
