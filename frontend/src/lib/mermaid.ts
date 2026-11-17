/** Cheap detection keeps the large Mermaid bundle out of ordinary code blocks. */
const MERMAID_START_RE =
  /^(?:graph|flowchart|sequenceDiagram|classDiagram|stateDiagram(?:-v2)?|erDiagram|journey|gantt|pie|quadrantChart|requirementDiagram|gitGraph|mindmap|timeline|zenuml|sankey-beta|xychart-beta|block-beta|packet-beta|kanban|architecture-beta|radar-beta|treemap-beta|venn-beta|ishikawa-beta|wardley-beta|cynefin-beta|treeView-beta|C4(?:Context|Container|Component|Dynamic|Deployment))\b/i;

function firstDefinitionLine(source: string): string {
  let text = source.trimStart();
  // Mermaid accepts YAML frontmatter before the diagram declaration.
  if (text.startsWith("---")) {
    text = text.replace(/^---\s*\n[\s\S]*?\n---\s*(?:\n|$)/, "");
  }
  // Directives and comments may also precede the first diagram keyword.
  text = text.replace(/^(?:\s*%%[^\n]*(?:\n|$))*/, "").trimStart();
  return text.split("\n", 1)[0] ?? "";
}

/**
 * Explicit `mermaid` fences always qualify. For an unlabeled fence, recognize
 * Mermaid's diagram declaration; never reinterpret a fence labeled as another
 * language.
 */
export function looksLikeMermaid(source: string, language = ""): boolean {
  const lang = language.trim().toLowerCase();
  if (lang === "mermaid") return true;
  if (lang !== "") return false;
  return MERMAID_START_RE.test(firstDefinitionLine(source));
}

let mermaidPromise: Promise<typeof import("mermaid").default> | null = null;
let renderTail: Promise<void> = Promise.resolve();

/**
 * The Mermaid bundle itself could not be fetched. Retrying re-imports it, which
 * succeeds in browsers that no longer cache failed module fetches; a page
 * reload is the fallback for those that still do.
 */
export class MermaidLoadError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = "MermaidLoadError";
  }
}

async function loadMermaid() {
  if (!mermaidPromise) {
    mermaidPromise = import("mermaid").then(
      ({ default: mermaid }) => {
        mermaid.initialize({
          startOnLoad: false,
          securityLevel: "strict",
          suppressErrorRendering: true,
          theme: "neutral",
          layout: "dagre",
          look: "classic",
        });
        return mermaid;
      },
      (err: unknown) => {
        throw new MermaidLoadError(
          err instanceof Error ? err.message : "Could not load Mermaid.",
          { cause: err },
        );
      },
    );
    // A failed chunk fetch or initialization must not be cached: let the next
    // render (or Retry) load the bundle again.
    mermaidPromise.catch(() => {
      mermaidPromise = null;
    });
  }
  return mermaidPromise;
}

/** Mermaid uses shared DOM state internally, so serialize on-demand renders. */
export function renderMermaidSvg(id: string, source: string): Promise<string> {
  const render = renderTail.then(async () => {
    const mermaid = await loadMermaid();
    // Let the parser's own diagnostic (line, expected token) reach the reader.
    await mermaid.parse(source);
    const { svg } = await mermaid.render(id, source);
    return svg;
  });
  // A failed diagram must not poison the queue for every later diagram.
  renderTail = render.then(
    () => undefined,
    () => undefined,
  );
  return render;
}

/**
 * Put generated SVG in a scriptless, network-isolated iframe. Mermaid's strict
 * mode already disables diagram links/HTML; the sandbox and CSP are a second
 * boundary for transcript content supplied by other users.
 */
export function mermaidFrameDocument(svg: string, fill = false): string {
  return `<!doctype html>
<html><head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; style-src 'unsafe-inline'; font-src data:">
<style>
html,body{margin:0;min-height:100%;background:#fff;color:#111827;color-scheme:light}
body{display:flex;align-items:center;justify-content:center;padding:16px;box-sizing:border-box}
svg{display:block;max-width:100%;height:auto}
${fill ? "html,body{width:100%;height:100%;min-height:0}body{padding:0}svg{width:100%!important;height:100%!important;max-width:none!important}" : ""}
</style>
</head><body>${svg}</body></html>`;
}

/** Natural SVG size, independent of the transcript column's width. */
export function mermaidDimensions(svg: string): {
  width: number;
  height: number;
} {
  const match =
    /\bviewBox=["']\s*[-\d.]+\s+[-\d.]+\s+([\d.]+)\s+([\d.]+)\s*["']/i.exec(
      svg,
    );
  if (!match) return { width: 800, height: 450 };
  const width = Number(match[1]);
  const height = Number(match[2]);
  if (
    !(width > 0) ||
    !(height > 0) ||
    !Number.isFinite(width) ||
    !Number.isFinite(height)
  )
    return { width: 800, height: 450 };
  return { width, height };
}

/** Best-effort responsive sizing for the inline preview. */
export function mermaidAspectRatio(svg: string): number {
  const { width, height } = mermaidDimensions(svg);
  return Math.max(0.6, Math.min(4, width / height));
}
