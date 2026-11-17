import type { Message } from "@/lib/types";
import { classifyCommandMessage } from "@/lib/commandMessages";
import { cn, formatNumber, fullTimestamp, humanizeTime } from "@/lib/utils";
import { BlockRenderer } from "./BlockRenderer";
import { Badge } from "./ui/badge";

function UsageFooter({ message }: { message: Message }) {
  const u = message.usage;
  const bits: string[] = [];
  if (message.model) bits.push(message.model);
  if (u) {
    if (u.input_tokens != null) bits.push(`${formatNumber(u.input_tokens)} in`);
    if (u.output_tokens != null)
      bits.push(`${formatNumber(u.output_tokens)} out`);
    if (u.cache_read_input_tokens)
      bits.push(`${formatNumber(u.cache_read_input_tokens)} cache read`);
  }
  if (!bits.length) return null;
  return (
    <div className="mt-2 flex flex-wrap gap-x-3 gap-y-1 text-[0.7rem] text-muted-foreground">
      {bits.map((b, i) => (
        <span key={i}>{b}</span>
      ))}
    </div>
  );
}

export function MessageBubble({
  message,
  results,
  runStart = true,
  runEnd = true,
}: {
  message: Message;
  results?: Map<string, string>;
  /**
   * Run-grouping flags for contiguous assistant messages of the same turn
   * (see `SessionDetail`). On the middle/last bubbles of a run we suppress the
   * repeated ASSISTANT label+timestamp (shown once at `runStart`); the usage
   * footer is shown once at `runEnd`. Defaults make a lone bubble self-contained.
   */
  runStart?: boolean;
  runEnd?: boolean;
}) {
  // CLI command-meta messages (slash-command invocations, caveats, command
  // output wrappers) arrive as `role:"user"` text but are noise — collapse them
  // to a compact, left-aligned muted chip with the full text on hover.
  const cmd = classifyCommandMessage(message);
  if (cmd) {
    return (
      <div className="flex w-full justify-start">
        <div
          className="flex max-w-[min(48rem,92%)] items-center gap-2 rounded-lg border border-border bg-muted/30 px-3 py-2 text-xs text-muted-foreground"
          title={cmd.raw}
        >
          <span className="truncate">{cmd.label}</span>
          {message.timestamp && (
            <span
              className="ml-auto whitespace-nowrap font-normal"
              title={fullTimestamp(message.timestamp)}
            >
              {humanizeTime(message.timestamp)}
            </span>
          )}
        </div>
      </div>
    );
  }

  // Tool results arrive as `role: "user"` in the Claude transcript (that's how
  // the API feeds them back), but they aren't the human speaking — render them
  // as a neutral, left-aligned "Tool result" instead of a User bubble.
  const isToolResult = message.kind === "tool_result";
  const isUser = message.role === "user" && !isToolResult;
  const sidechain = message.is_sidechain;
  const label = isToolResult ? "Tool result" : isUser ? "User" : "Assistant";

  // Within an assistant run, show the label+timestamp header only on the first
  // bubble and the usage footer only on the last; suppress both on the middle
  // bubbles so a turn reads as one block rather than N repeated headers/footers.
  const showHeader = runStart;
  const showFooter = message.role === "assistant" && runEnd;

  return (
    <div
      className={cn(
        "flex w-full",
        isUser ? "justify-end" : "justify-start",
        sidechain && "pl-6 sm:pl-10",
      )}
    >
      <div
        className={cn(
          "conversation-bubble max-w-[min(80rem,92%)] rounded-2xl border px-4 py-3 shadow-xs",
          isUser
            ? "rounded-br-md border-primary/30 bg-primary/10"
            : isToolResult
              ? "rounded-bl-md border-border bg-muted/40"
              : "rounded-bl-md border-border bg-card",
          sidechain && "border-dashed",
        )}
      >
        {showHeader && (
          <div className="mb-1.5 flex items-center gap-2 text-[0.7rem] font-medium uppercase tracking-wide text-muted-foreground">
            <span>{label}</span>
            {sidechain && (
              <Badge variant="outline" className="py-0 text-[0.65rem]">
                sub-agent
              </Badge>
            )}
            {message.timestamp && (
              <span
                className="ml-auto font-normal normal-case"
                title={fullTimestamp(message.timestamp)}
              >
                {humanizeTime(message.timestamp)}
              </span>
            )}
          </div>
        )}

        <div className="space-y-2">
          {message.blocks.length ? (
            message.blocks.map((block, i) => (
              <BlockRenderer key={i} block={block} results={results} />
            ))
          ) : (
            <p className="text-sm text-muted-foreground">(empty message)</p>
          )}
        </div>

        {showFooter && <UsageFooter message={message} />}
      </div>
    </div>
  );
}
