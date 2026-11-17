import { looksLikeMermaid } from "./mermaid";
import type { Message } from "./types";

export interface MermaidSource {
  id: string;
  source: string;
}

/** Read complete Mermaid fences from agent replies, never from terminal glyphs or tool output. */
export function mermaidSources(messages: readonly Message[]): MermaidSource[] {
  const found: MermaidSource[] = [];
  for (const message of messages) {
    if (message.role !== "assistant" || message.is_sidechain) continue;
    const text = message.blocks
      .filter((block) => block.type === "text")
      .map((block) => block.text)
      .join("\n");
    let fence: string | undefined;
    let language = "";
    let lines: string[] = [];
    let index = 0;
    for (const line of text.split(/\r?\n/)) {
      if (!fence) {
        const start = /^ {0,3}(`{3,}|~{3,})(.*)$/.exec(line);
        if (!start) continue;
        fence = start[1];
        language = start[2].trim().split(/\s+/, 1)[0];
        lines = [];
      } else if (
        new RegExp(`^ {0,3}${fence[0]}{${fence.length},}\\s*$`).test(line)
      ) {
        const source = lines.join("\n");
        if (source.trim() && looksLikeMermaid(source, language))
          found.push({ id: `${message.uuid}:${index}`, source });
        index++;
        fence = undefined;
      } else lines.push(line);
    }
  }
  return found;
}
