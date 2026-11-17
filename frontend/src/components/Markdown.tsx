import * as React from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import rehypeRaw from "rehype-raw";
import rehypeSanitize, { defaultSchema } from "rehype-sanitize";
import type { Root } from "hast";
import { CodeBlock } from "./CodeBlock";
import { MarkdownBlock } from "./MarkdownBlock";
import { MermaidBlock } from "./MermaidBlock";
import { rehypeLanguages } from "@/lib/highlight";
import { looksLikeMermaid } from "@/lib/mermaid";
import { extractPrUrls, type PrRef } from "@/lib/prUrls";
import { findPrRefs, type RepoContext } from "@/lib/prRefs";
import { DetectedPr, usePrDetect } from "./PrDetect";
import "highlight.js/styles/github-dark.css";

const MARKDOWN_LANGUAGES = new Set([
  "markdown",
  "md",
  "mdown",
  "mkdown",
  "mkd",
]);

/** Extract plain text from arbitrary React children (for copy button). */
function childrenToText(children: React.ReactNode): string {
  if (children == null) return "";
  if (typeof children === "string" || typeof children === "number")
    return String(children);
  if (Array.isArray(children)) return children.map(childrenToText).join("");
  if (React.isValidElement(children))
    return childrenToText(
      (children.props as { children?: React.ReactNode }).children,
    );
  return "";
}

/** Parse a single full PR URL into a `PrRef`, or null if it isn't one. */
function prRefFromUrl(href: string): PrRef | null {
  return extractPrUrls(href)[0] ?? null;
}

/**
 * rehype plugin: wrap short PR references (`PR #193`, `PR 193`) found in text
 * nodes in a custom `<pr-ref>` element carrying the resolved PR as JSON. Full
 * URLs are handled by the `a` component override (GFM autolinks them), so this
 * skips text inside `<a>` — and inside `<code>`/`<pre>` to leave code alone.
 *
 * Only added to the pipeline when detection is enabled (see `Markdown`).
 */
function rehypePrRefs(repoCtx: RepoContext | null) {
  const SKIP = new Set(["a", "code", "pre"]);

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  function walk(nodes: any[]): any[] {
    const out: any[] = [];
    for (const node of nodes) {
      if (node?.type === "text" && typeof node.value === "string") {
        const refs = findPrRefs(node.value, repoCtx);
        if (refs.length) {
          let cursor = 0;
          for (const m of refs) {
            if (m.start > cursor) {
              out.push({
                type: "text",
                value: node.value.slice(cursor, m.start),
              });
            }
            out.push({
              type: "element",
              tagName: "pr-ref",
              properties: { prref: JSON.stringify(m.ref) },
              children: [{ type: "text", value: m.raw }],
            });
            cursor = m.end;
          }
          if (cursor < node.value.length) {
            out.push({ type: "text", value: node.value.slice(cursor) });
          }
          continue;
        }
      }
      if (
        node?.type === "element" &&
        Array.isArray(node.children) &&
        !SKIP.has(node.tagName)
      ) {
        node.children = walk(node.children);
      }
      out.push(node);
    }
    return out;
  }

  return (tree: Root) => {
    tree.children = walk(tree.children);
  };
}

/**
 * Sanitize schema for `allowHtml` mode. Based on the GitHub-flavoured default
 * (which already drops `<script>`, event handlers, `javascript:` URLs, and —
 * crucially — every HTML comment, so CodeRabbit's `<!-- internal state … -->`,
 * `<!-- linear-linkback -->`, and `<!-- {"checkboxId":…} -->` noise is stripped).
 * We only widen it to permit GitHub's collapsible `<details>`/`<summary>` and
 * the `open` attribute so review/issue bodies render like they do on GitHub.
 */
const htmlSchema = {
  ...defaultSchema,
  tagNames: [...(defaultSchema.tagNames ?? []), "details", "summary"],
  attributes: {
    ...defaultSchema.attributes,
    details: [...(defaultSchema.attributes?.details ?? []), "open"],
  },
};

const components: Components = {
  // <pre> wraps fenced code; we render our own chrome via CodeBlock and let
  // the inner <code> keep rehype-highlight's hljs classes.
  pre({ children }) {
    let language = "";
    let codeChild: React.ReactNode = children;
    if (React.isValidElement(children)) {
      const cls: string =
        (children.props as { className?: string }).className ?? "";
      const m = /language-([\w-]+)/.exec(cls);
      if (m) language = m[1];
      codeChild = children;
    }
    const text = childrenToText(children);
    if (looksLikeMermaid(text, language)) {
      return <MermaidBlock source={text} />;
    }
    if (MARKDOWN_LANGUAGES.has(language.toLowerCase())) {
      return (
        <MarkdownBlock source={text} preview={<Markdown>{text}</Markdown>} />
      );
    }
    return (
      <CodeBlock code={text} language={language}>
        {codeChild}
      </CodeBlock>
    );
  },
  // Open links safely in a new tab. A link whose href is a GitHub PR URL is
  // wrapped in a "detected" chip (a no-op passthrough when detection is off).
  a({ href, children, ...rest }) {
    const anchor = (
      <a href={href} target="_blank" rel="noopener noreferrer" {...rest}>
        {children}
      </a>
    );
    const ref = href ? prRefFromUrl(href) : null;
    return ref ? <DetectedPr prRef={ref}>{anchor}</DetectedPr> : anchor;
  },
  // Custom element emitted by `rehypePrRefs` for short `PR #N` references.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  "pr-ref": ({ node, children }: any) => {
    let ref: PrRef | null = null;
    try {
      ref = JSON.parse(node?.properties?.prref ?? "null");
    } catch {
      // Keep the null fallback for malformed embedded metadata.
    }
    if (!ref) return <>{children}</>;
    return <DetectedPr prRef={ref}>{children}</DetectedPr>;
  },
} as Components;

export const Markdown = React.memo(function Markdown({
  children,
  allowHtml = false,
}: {
  children: string;
  /**
   * Render embedded HTML (sanitized) instead of showing it as literal text.
   * Opt-in — used for GitHub comment bodies, which embed `<details>` blocks,
   * `<a>` links, and HTML comments. Off everywhere else (transcripts etc.) so
   * the default render surface stays raw-HTML-free. Always runs through
   * `rehype-sanitize`, so this adds no XSS surface.
   */
  allowHtml?: boolean;
}) {
  // Detection is opt-in via context (only the session views provide it). When
  // enabled, append the short-`PR #N` detector; full URLs are handled by the
  // `a` override regardless (a no-op chip when detection is off).
  const { enabled, repoCtx } = usePrDetect();
  const rehypePlugins = React.useMemo(() => {
    const base: NonNullable<
      React.ComponentProps<typeof ReactMarkdown>["rehypePlugins"]
    > = [];
    // rehype-raw reparses embedded HTML into real nodes; rehype-sanitize then
    // strips anything unsafe (and all comments). Both must run BEFORE highlight
    // and the PR-ref pass so those plugins' own output stays trusted.
    if (allowHtml) {
      base.push(rehypeRaw, [rehypeSanitize, htmlSchema]);
    }
    base.push([
      rehypeHighlight,
      { detect: false, ignoreMissing: true, languages: rehypeLanguages },
    ]);
    if (enabled) base.push([rehypePrRefs, repoCtx]);
    return base;
  }, [enabled, repoCtx, allowHtml]);

  return (
    <div className="prose-chat">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        // `detect: false` — never guess a language for an unlabeled fence
        // (auto-detection mislabels e.g. a shell command as "swift"). Blocks
        // with an explicit language still get highlighted + labeled.
        // Register the same extra grammars (hcl, toml-via-ini, …) so fenced
        // blocks match the direct-highlight path in CodeBlock and don't fall
        // back to plain text for non-"common" languages.
        rehypePlugins={rehypePlugins}
        components={components}
      >
        {children}
      </ReactMarkdown>
    </div>
  );
});
