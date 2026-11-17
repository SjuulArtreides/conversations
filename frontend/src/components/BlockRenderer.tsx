import * as React from "react";
import { diffLines } from "diff";
import type { ContentBlock } from "@/lib/types";
import { isErrorResult } from "@/lib/toolResults";
import { cn } from "@/lib/utils";
import { Markdown } from "./Markdown";
import { Collapsible } from "./Collapsible";
import { CodeBlock } from "./CodeBlock";
import { MermaidBlock } from "./MermaidBlock";
import { DetectedInline } from "./PrDetect";

const TRUNCATE_AT = 4000;
/** Results at or below this length (and not multi-line) render inline. */
const INLINE_RESULT_MAX = 120;

function TruncatedPre({
  content,
  language = "text",
}: {
  content: string;
  language?: string;
}) {
  const [expanded, setExpanded] = React.useState(false);
  const long = content.length > TRUNCATE_AT;
  const shown = expanded || !long ? content : content.slice(0, TRUNCATE_AT);
  return (
    <div>
      {language === "mermaid" && shown === content ? (
        <MermaidBlock source={content} />
      ) : (
        <CodeBlock code={shown} language={language} />
      )}
      {long && (
        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          className="mt-1 text-xs font-medium text-primary hover:underline"
        >
          {expanded
            ? "Show less"
            : `Show more (${(content.length - TRUNCATE_AT).toLocaleString()} more chars)`}
        </button>
      )}
    </div>
  );
}

/** Map a file path to a highlight.js language hint via its extension. */
export function langForPath(path: string): string {
  const name = path.split("/").pop() ?? path;
  if (/^Dockerfile$/i.test(name) || /\.dockerfile$/i.test(name))
    return "dockerfile";
  if (/^Makefile$/i.test(name) || /\.mk$/i.test(name)) return "makefile";
  const ext = name.includes(".") ? name.split(".").pop()!.toLowerCase() : "";
  const table: Record<string, string> = {
    ts: "typescript",
    tsx: "typescript",
    js: "javascript",
    jsx: "javascript",
    mjs: "javascript",
    cjs: "javascript",
    rs: "rust",
    py: "python",
    json: "json",
    md: "markdown",
    markdown: "markdown",
    yaml: "yaml",
    yml: "yaml",
    toml: "toml",
    sh: "bash",
    bash: "bash",
    zsh: "bash",
    html: "html",
    htm: "html",
    css: "css",
    sql: "sql",
    hcl: "hcl",
    tf: "hcl",
    tfvars: "hcl",
    go: "go",
    nix: "nix",
    diff: "diff",
    patch: "diff",
    graphql: "graphql",
    gql: "graphql",
    java: "java",
    c: "c",
    h: "c",
    cpp: "cpp",
    cc: "cpp",
    cxx: "cpp",
    hpp: "cpp",
    cs: "csharp",
    rb: "ruby",
    php: "php",
    kt: "kotlin",
    kts: "kotlin",
    swift: "swift",
    scala: "scala",
    sc: "scala",
    proto: "protobuf",
    mmd: "mermaid",
    mermaid: "mermaid",
  };
  return table[ext] ?? "text";
}

function str(input: Record<string, unknown>, key: string): string {
  const v = input[key];
  return typeof v === "string" ? v : "";
}

/** A syntax-highlighted code box rendered through the Markdown pipeline. */
function FencedCode({ code, language }: { code: string; language: string }) {
  // Pick a fence longer than any backtick run in the payload.
  const fence = code.includes("```") ? "````" : "```";
  return <Markdown>{`${fence}${language}\n${code}\n${fence}`}</Markdown>;
}

/**
 * A unified line diff between two strings: removed lines tinted red with a `-`
 * gutter, added lines tinted green with a `+` gutter, unchanged lines neutral.
 * Styled to match the existing code boxes (rounded, bordered, mono, scrollable).
 */
function DiffView({ oldText, newText }: { oldText: string; newText: string }) {
  type Row = { gutter: string; text: string; kind: "add" | "del" | "ctx" };
  const rows: Row[] = [];
  for (const part of diffLines(oldText, newText)) {
    const kind: Row["kind"] = part.added ? "add" : part.removed ? "del" : "ctx";
    const gutter = kind === "add" ? "+" : kind === "del" ? "-" : " ";
    // `diffLines` emits trailing-newline-terminated chunks; drop the empty
    // tail so we don't render a phantom final row.
    const lines = part.value.split("\n");
    if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
    for (const text of lines) rows.push({ gutter, text, kind });
  }

  return (
    <div className="rounded-lg border border-border bg-muted/20 text-xs">
      <pre className="font-mono leading-relaxed">
        {rows.map((row, i) => (
          <div
            key={i}
            className={cn(
              "flex px-3",
              row.kind === "add" && "bg-green-500/10 text-green-300",
              row.kind === "del" && "bg-red-500/10 text-red-300",
              row.kind === "ctx" && "text-muted-foreground",
            )}
          >
            <span className="shrink-0 select-none opacity-60">
              {row.gutter}&nbsp;
            </span>
            {/* Soft-wrap long edited lines instead of forcing horizontal
                scroll: wrap on whitespace and break overflowing tokens. */}
            <span className="min-w-0 whitespace-pre-wrap wrap-break-word">
              {row.text}
            </span>
          </div>
        ))}
      </pre>
    </div>
  );
}

/**
 * The folded-in result of a tool call, shown inside the same box. When `bare`
 * is set the multi-line branch renders the content directly (no inner
 * Collapsible) — used by chip tools whose outer collapsible already identifies
 * the call, so a nested "→ result" disclosure would just double the nesting.
 */
function ToolResult({
  content,
  bare = false,
}: {
  content: string;
  bare?: boolean;
}) {
  const trimmed = content.trim();
  if (!trimmed) return null;
  const isError = isErrorResult(trimmed);
  const inline =
    !isError && trimmed.length <= INLINE_RESULT_MAX && !trimmed.includes("\n");

  if (inline) {
    return (
      <div className="mt-2 text-xs text-muted-foreground">
        <span className="font-mono">→ </span>
        {trimmed}
      </div>
    );
  }

  // Bare mode: render the content directly, skipping the redundant inner
  // disclosure. Errors stay tinted red for visual distinction.
  if (bare) {
    return (
      <div className={cn("mt-2", isError && "text-red-400")}>
        <TruncatedPre content={content} />
      </div>
    );
  }

  // Size hint on the folded label so a collapsed result advertises its bulk.
  const lineCount = trimmed.split("\n").length;
  return (
    <div className="mt-2">
      <Collapsible
        label={
          <span className={isError ? "text-red-400" : undefined}>
            → {isError ? "error" : "result"}
            <span className="ml-1 font-normal text-muted-foreground">
              · {lineCount.toLocaleString()} line{lineCount === 1 ? "" : "s"}
            </span>
          </span>
        }
      >
        <TruncatedPre content={content} />
      </Collapsible>
    </div>
  );
}

/** Header label for a tool-call box: 🔧 <Name>[: <subject>] [subagent]. */
function ToolLabel({
  name,
  subject,
  badge,
}: {
  name: string;
  subject?: string;
  badge?: string;
}) {
  return (
    <span>
      🔧 <span className="font-mono text-foreground">{name}</span>
      {subject && (
        <span className="text-muted-foreground">
          : <DetectedInline text={subject} />
        </span>
      )}
      {badge && (
        <span className="ml-2 rounded bg-muted px-1.5 py-0.5 text-[0.65rem] font-medium text-muted-foreground">
          {badge}
        </span>
      )}
    </span>
  );
}

/** A non-collapsible single-line chip (header only, no body, no JSON). */
function ToolChip({ name, subject }: { name: string; subject?: string }) {
  return (
    <div className="rounded-lg border border-border bg-muted/30 px-3 py-2 text-xs font-medium text-muted-foreground">
      <ToolLabel name={name} subject={subject} />
    </div>
  );
}

/**
 * Render a `tool_use` block: a compact header plus an optional body, with the
 * paired tool result (if any) folded into the same box.
 */
function ToolUse({
  block,
  results,
}: {
  block: Extract<ContentBlock, { type: "tool_use" }>;
  results?: Map<string, string>;
}) {
  const input = (block.input ?? {}) as Record<string, unknown>;
  const result = results?.get(block.id);
  const resultNode = result != null ? <ToolResult content={result} /> : null;

  // A box (collapsible) with a body + optional folded result. The Bash case
  // opens by default so the command is visible without a click; every other
  // caller stays closed.
  const box = (
    label: React.ReactNode,
    body: React.ReactNode,
    defaultOpen = false,
  ) => (
    <Collapsible label={label} defaultOpen={defaultOpen}>
      {body}
      {resultNode}
    </Collapsible>
  );

  // A header-only chip; if a result exists, fold it into a tiny box instead so
  // the result stays reachable. The result renders `bare` so opening the single
  // outer collapsible reveals the content directly — no nested disclosure.
  const chip = (subject?: string) => {
    // `resultNode` is truthy only when `result` is a non-empty string, so the
    // `?? ""` is just to satisfy the type checker.
    if (!resultNode) return <ToolChip name={block.name} subject={subject} />;
    return (
      <Collapsible label={<ToolLabel name={block.name} subject={subject} />}>
        <ToolResult content={result ?? ""} bare />
      </Collapsible>
    );
  };

  switch (block.name) {
    case "Bash": {
      const command = str(input, "command");
      const subject = str(input, "description");
      if (!command) break;
      return box(
        <ToolLabel name="Bash" subject={subject} />,
        <FencedCode code={command} language="bash" />,
        true,
      );
    }

    case "Write": {
      const filePath = str(input, "file_path");
      const content = str(input, "content");
      const lang = langForPath(filePath);
      return box(
        <ToolLabel name="Write" subject={filePath} />,
        <TruncatedPre content={content} language={lang} />,
      );
    }

    case "Edit": {
      const filePath = str(input, "file_path");
      const oldStr = str(input, "old_string");
      const newStr = str(input, "new_string");
      return box(
        <ToolLabel name="Edit" subject={filePath} />,
        <DiffView oldText={oldStr} newText={newStr} />,
      );
    }

    case "MultiEdit": {
      const filePath = str(input, "file_path");
      const edits = Array.isArray(input.edits) ? input.edits : null;
      const valid =
        edits &&
        edits.every(
          (e) =>
            e &&
            typeof e === "object" &&
            typeof (e as Record<string, unknown>).old_string === "string" &&
            typeof (e as Record<string, unknown>).new_string === "string",
        );
      if (valid) {
        return box(
          <ToolLabel name="MultiEdit" subject={filePath} />,
          <div className="space-y-3">
            {(edits as Record<string, unknown>[]).map((e, i) => (
              <div key={i} className="space-y-1">
                <div className="text-[0.7rem] font-medium uppercase tracking-wide text-muted-foreground">
                  Edit #{i + 1}
                </div>
                <DiffView
                  oldText={e.old_string as string}
                  newText={e.new_string as string}
                />
              </div>
            ))}
          </div>,
        );
      }
      // Awkward shape — compact JSON fallback.
      const json = JSON.stringify(block.input ?? {}, null, 2);
      return box(
        <ToolLabel name="MultiEdit" subject={filePath} />,
        <CodeBlock code={json} language="json">
          <code className="hljs language-json">{json}</code>
        </CodeBlock>,
      );
    }

    case "Read":
      return chip(str(input, "file_path"));

    case "Glob": {
      const pattern = str(input, "pattern");
      const path = str(input, "path");
      return chip(path ? `${pattern} (${path})` : pattern);
    }

    case "Grep":
      return chip(str(input, "pattern"));

    case "Skill":
      return chip(str(input, "skill"));

    case "WebFetch":
      return chip(str(input, "url"));

    case "WebSearch":
      return chip(str(input, "query"));

    case "Task": {
      const subject = str(input, "description");
      const prompt = str(input, "prompt");
      const subagent = str(input, "subagent_type");
      return box(
        <ToolLabel
          name="Task"
          subject={subject}
          badge={subagent || undefined}
        />,
        prompt ? (
          <div className="text-muted-foreground">
            <Markdown>{prompt}</Markdown>
          </div>
        ) : null,
      );
    }

    case "TodoWrite": {
      const todos = Array.isArray(input.todos) ? input.todos : [];
      if (!todos.length) return chip("0 items");
      const mark = (status: unknown) =>
        status === "completed" ? "☑" : status === "in_progress" ? "◐" : "☐";
      return box(
        <ToolLabel name="TodoWrite" subject={`${todos.length} items`} />,
        <ul className="space-y-1 text-sm">
          {(todos as Record<string, unknown>[]).map((t, i) => {
            const content =
              typeof t.content === "string"
                ? t.content
                : typeof t.activeForm === "string"
                  ? (t.activeForm as string)
                  : JSON.stringify(t);
            return (
              <li key={i} className="flex gap-2">
                <span aria-hidden>{mark(t.status)}</span>
                <span
                  className={
                    t.status === "completed"
                      ? "text-muted-foreground line-through"
                      : undefined
                  }
                >
                  {content}
                </span>
              </li>
            );
          })}
        </ul>,
      );
    }
  }

  // Default / unknown tool: subject = description if any, JSON box body.
  const description = str(input, "description");
  const json = JSON.stringify(block.input ?? {}, null, 2);
  return box(
    <ToolLabel name={block.name} subject={description || undefined} />,
    <CodeBlock code={json} language="json">
      <code className="hljs language-json">{json}</code>
    </CodeBlock>,
  );
}

export function BlockRenderer({
  block,
  results,
}: {
  block: ContentBlock;
  results?: Map<string, string>;
}) {
  switch (block.type) {
    case "text":
      return <Markdown>{block.text}</Markdown>;

    case "thinking": {
      // Claude Code often stores extended-thinking blocks with only an
      // encrypted signature and an empty `thinking` string — the plaintext is
      // never written to the on-disk transcript. Those blocks carry zero
      // information for a reader, so drop them entirely.
      const hasText = block.thinking.trim().length > 0;
      if (!hasText) {
        return null;
      }
      // Text-bearing thinking blocks remain collapsed by default (Collapsible
      // defaults to closed) so they don't clutter the transcript.
      return (
        <Collapsible label={<span>💭 Thinking</span>}>
          <div className="text-muted-foreground">
            <Markdown>{block.thinking}</Markdown>
          </div>
        </Collapsible>
      );
    }

    case "tool_use":
      return <ToolUse block={block} results={results} />;

    case "tool_result":
      // Normally folded into the originating call box (and the pure-result
      // message is filtered out upstream). This standalone rendering remains
      // for messages that MIX a tool_result with other content.
      return (
        <Collapsible label={<span>⬅ result</span>}>
          <TruncatedPre content={block.content ?? ""} />
        </Collapsible>
      );

    case "other":
    default:
      return (
        <p className="text-xs italic text-muted-foreground">
          (unsupported block)
        </p>
      );
  }
}
