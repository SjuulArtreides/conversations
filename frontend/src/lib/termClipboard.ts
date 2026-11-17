/**
 * Build Slack-friendly clipboard payloads from an xterm.js selection.
 *
 * The live terminal renders a raw PTY byte stream via xterm's WebGL renderer
 * (`LiveTerminal.tsx`). By the time text is on screen there is no semantic
 * structure left — "this is inline code" only survives as the per-cell ANSI
 * foreground colour that Claude Code's TUI used to draw it. xterm's own copy
 * handler writes plain text only, dropping even that signal.
 *
 * This module walks the selected buffer cells, reconstructs runs that carry the
 * configured "code" foreground colour, and emits BOTH a `text/plain` flavour
 * (code runs wrapped in backticks) and a `text/html` flavour (code runs wrapped
 * in `<code>`). Slack's rich-text composer reads the HTML flavour and renders
 * those `<code>` runs as inline code — restyled in its own palette, so the blue
 * does not carry, only the code *formatting* does. That is the intended result.
 *
 * Colour is the only signal, so which fg colour means "inline code" must be
 * pinned to Claude Code's actual TUI palette — see {@link resolveCodeColors} and
 * {@link selectionColorSignatures} for the one-time discovery path.
 */

/** Minimal structural view of an xterm `IBufferCell` (so tests can fake it). */
export interface CellLike {
  getChars(): string;
  getWidth(): number;
  getFgColor(): number;
  isFgDefault(): boolean;
  isFgPalette(): boolean;
  isFgRGB(): boolean;
}

/** Minimal structural view of an xterm `IBufferLine`. */
export interface LineLike {
  readonly length: number;
  /** True when this row is the soft-wrapped continuation of the row above. */
  readonly isWrapped: boolean;
  getCell(x: number, cell?: CellLike): CellLike | undefined;
}

/** Minimal structural view of the parts of `Terminal` we read. */
export interface TermLike {
  getSelectionPosition():
    | { start: { x: number; y: number }; end: { x: number; y: number } }
    | undefined;
  buffer: { active: { getLine(y: number): LineLike | undefined } };
}

/**
 * Foreground colours that mean "inline code". Palette entries are ANSI/256
 * indices; RGB entries are packed `0xRRGGBB` (24-bit truecolor SGR). Both are
 * matched against `IBufferCell.getFgColor()` for the corresponding colour mode.
 */
export interface CodeColorConfig {
  palette: ReadonlySet<number>;
  rgb: ReadonlySet<number>;
}

/**
 * Pinned colours for Claude Code's inline code, discovered from a real live
 * session (see {@link selectionColorSignatures}). `0xb1b9f9` is the truecolor
 * (24-bit) foreground Claude Code's TUI draws inline `code` with — verified by
 * copy→Slack against a live session. It is a property of Claude's TUI output
 * (the same for every viewer), not a per-user preference, which is why a single
 * baked-in default is the right home for it rather than per-user config.
 *
 * If Claude's theme changes, or for a different agent's palette (e.g. Codex),
 * retune WITHOUT a rebuild by setting `localStorage.arrakisTermCodeColors` to
 * JSON of the shape `{ "palette": [..], "rgb": [..] }` — see
 * {@link resolveCodeColors}; the override fully replaces this default.
 */
export const DEFAULT_CODE_COLORS: CodeColorConfig = {
  palette: new Set<number>(),
  rgb: new Set<number>([0xb1b9f9]),
};

const LS_OVERRIDE_KEY = "arrakisTermCodeColors";
const LS_DEBUG_KEY = "arrakisTermClipboardDebug";
const LS_GITHUB_ORG_KEY = "arrakisGithubOrg";

/**
 * Default GitHub org for resolving the bare `repo#n` PR shorthand, in
 * precedence order:
 *   1. `localStorage.arrakisGithubOrg` (manual override / escape hatch), else
 *   2. `inferred` — the caller passes the org of a PR already linked to the
 *      session here, so the common case (`example-deploy#1155` in a session
 *      whose PRs live under the same org) resolves with zero config.
 * Returns `undefined` when neither is available — `repo#n` then stays plain
 * text (`owner/repo#n` and full URLs still link without an org).
 */
export function resolveGithubOrg(
  storage: Pick<Storage, "getItem"> | undefined = safeLocalStorage(),
  inferred?: string,
): string | undefined {
  const override = storage?.getItem(LS_GITHUB_ORG_KEY)?.trim();
  if (override) return override;
  return inferred?.trim() || undefined;
}

/**
 * Effective code colours, in precedence order:
 *   1. `localStorage.arrakisTermCodeColors` JSON override (dev / quick-retune
 *      escape hatch — wins so a developer can pin a colour without touching the
 *      saved per-user setting), else
 *   2. `fallback` — the caller passes the user's saved server setting here, and
 *      it defaults to {@link DEFAULT_CODE_COLORS} (the baked-in Claude colour).
 * RGB values accept numbers or `"#rrggbb"`/`"rrggbb"` strings.
 */
export function resolveCodeColors(
  storage: Pick<Storage, "getItem"> | undefined = safeLocalStorage(),
  fallback: CodeColorConfig = DEFAULT_CODE_COLORS,
): CodeColorConfig {
  const raw = storage?.getItem(LS_OVERRIDE_KEY);
  if (!raw) return fallback;
  try {
    const parsed = JSON.parse(raw) as { palette?: unknown; rgb?: unknown };
    return {
      palette: new Set(toNumbers(parsed.palette)),
      rgb: new Set(toNumbers(parsed.rgb)),
    };
  } catch {
    return fallback;
  }
}

/**
 * Convert the server-side per-user colour DTO (`{ palette?, rgb? }`) into a
 * {@link CodeColorConfig}, or `null` when unset — callers then fall through to
 * {@link DEFAULT_CODE_COLORS}.
 */
export function codeColorsFromDto(
  dto: { palette?: number[]; rgb?: number[] } | null | undefined,
): CodeColorConfig | null {
  if (!dto) return null;
  return {
    palette: new Set(toNumbers(dto.palette)),
    rgb: new Set(toNumbers(dto.rgb)),
  };
}

function toNumbers(v: unknown): number[] {
  if (!Array.isArray(v)) return [];
  const out: number[] = [];
  for (const item of v) {
    if (typeof item === "number" && Number.isFinite(item)) out.push(item);
    else if (typeof item === "string") {
      const n = parseInt(item.replace(/^#/, ""), 16);
      if (Number.isFinite(n)) out.push(n);
    }
  }
  return out;
}

function safeLocalStorage(): Pick<Storage, "getItem"> | undefined {
  try {
    return typeof localStorage !== "undefined" ? localStorage : undefined;
  } catch {
    return undefined;
  }
}

/** Whether a cell's foreground colour marks it as inline code. */
function isCodeCell(cell: CellLike, cfg: CodeColorConfig): boolean {
  if (cell.isFgDefault()) return false;
  if (cell.isFgPalette()) return cfg.palette.has(cell.getFgColor());
  if (cell.isFgRGB()) return cfg.rgb.has(cell.getFgColor());
  return false;
}

interface VisualCell {
  ch: string;
  code: boolean;
}

/**
 * Walk the selection into logical lines of {@link VisualCell}s. Soft-wrapped
 * rows (xterm's `isWrapped`) are merged back into one logical line so a wrapped
 * code run stays a single run. Returns `null` when there is no selection.
 */
function collectLogicalLines(
  term: TermLike,
  cfg: CodeColorConfig,
): VisualCell[][] | null {
  const range = term.getSelectionPosition();
  if (!range) return null;

  const logical: VisualCell[][] = [];
  for (let y = range.start.y; y <= range.end.y; y++) {
    const line = term.buffer.active.getLine(y);
    if (!line) continue;
    const colStart = y === range.start.y ? range.start.x : 0;
    // xterm's selection end column is exclusive; non-final rows run to the
    // line's full width.
    const colEnd = y === range.end.y ? range.end.x : line.length;

    const cells: VisualCell[] = [];
    for (let x = colStart; x < colEnd; x++) {
      const cell = line.getCell(x);
      if (!cell) continue;
      // A width-0 cell is the spacer trailing a wide glyph — its char is "" and
      // the glyph itself already came from the preceding cell. Skip it.
      if (cell.getWidth() === 0) continue;
      const chars = cell.getChars();
      cells.push({
        ch: chars === "" ? " " : chars,
        code: isCodeCell(cell, cfg),
      });
    }

    // `isWrapped` (and a non-empty accumulator) means this row continues the
    // previous logical line — append without a line break.
    if (line.isWrapped && logical.length > 0) {
      logical[logical.length - 1].push(...cells);
    } else {
      logical.push(cells);
    }
  }
  return logical;
}

/** Drop trailing whitespace cells that are NOT code (mirrors xterm trimRight). */
function trimTrailing(cells: VisualCell[]): VisualCell[] {
  let end = cells.length;
  while (end > 0 && !cells[end - 1].code && /^\s*$/.test(cells[end - 1].ch)) {
    end--;
  }
  return cells.slice(0, end);
}

function escapeHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/** Escape a string for use inside a double-quoted HTML attribute value. */
function escapeAttr(s: string): string {
  return escapeHtml(s).replace(/"/g, "&quot;");
}

const GITHUB = "https://github.com";

/**
 * GitHub PR / issue references, in priority order (the regex is tried
 * left-to-right at each position, so the longest/most-specific form wins):
 *   1. a full `github.com/<owner>/<repo>/(pull|issues)/<n>` URL,
 *   2. an `<owner>/<repo>#<n>` shorthand, or
 *   3. a bare `<repo>#<n>` shorthand (needs a default org to resolve — see
 *      {@link linkifyHtml}).
 * `/pull/<n>` is used for every form; GitHub redirects it to `/issues/<n>` when
 * the number is an issue, so a single target URL works for both.
 */
const PR_REF_RE =
  /(https?:\/\/github\.com\/[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+\/(?:pull|issues)\/\d+(?:\/[A-Za-z0-9._/-]*)?)|([A-Za-z0-9._-]+\/[A-Za-z0-9._-]+#\d+)|([A-Za-z0-9._-]+#\d+)/g;

/**
 * Escape `raw` for HTML, additionally turning GitHub PR/issue references into
 * `<a>` links. `owner/repo#n` and full URLs resolve with no config; the bare
 * `repo#n` shorthand resolves only when an `org` is supplied (else it is left as
 * escaped text). Only called on non-code spans — code runs are never linkified.
 */
function linkifyHtml(raw: string, org?: string): string {
  let out = "";
  let last = 0;
  for (const m of raw.matchAll(PR_REF_RE)) {
    const idx = m.index ?? 0;
    out += escapeHtml(raw.slice(last, idx));
    const full = m[0];
    let href: string | null = null;
    if (m[1]) {
      href = m[1]; // already a full github.com PR/issue URL
    } else if (m[2]) {
      const [slug, num] = m[2].split("#"); // owner/repo#n
      href = `${GITHUB}/${slug}/pull/${num}`;
    } else if (m[3] && org) {
      const [repo, num] = m[3].split("#"); // repo#n + default org
      href = `${GITHUB}/${org}/${repo}/pull/${num}`;
    }
    out += href
      ? `<a href="${escapeAttr(href)}">${escapeHtml(full)}</a>`
      : escapeHtml(full);
    last = idx + full.length;
  }
  out += escapeHtml(raw.slice(last));
  return out;
}

/** Emit `text/plain` (backtick-wrapped) + `text/html` (`<code>`) for one line. */
function renderLine(
  cells: VisualCell[],
  org?: string,
): { text: string; html: string } {
  let text = "";
  let html = "";
  let i = 0;
  while (i < cells.length) {
    const code = cells[i].code;
    let run = "";
    while (i < cells.length && cells[i].code === code) {
      run += cells[i].ch;
      i++;
    }
    if (code) {
      // A run containing a backtick can't be fenced with a single backtick
      // without breaking the markdown; leave it un-fenced in plain text (the
      // HTML flavour — what Slack actually reads — still marks it as <code>).
      text += run.includes("`") ? run : "`" + run + "`";
      html += "<code>" + escapeHtml(run) + "</code>";
    } else {
      text += run;
      html += linkifyHtml(run, org);
    }
  }
  return { text, html };
}

const BULLET_MARKER_RE = /^[-*•]$/;

/**
 * Detect a leading list-bullet marker (`-`, `*`, or `•` followed by a space,
 * after any indentation). Returns the cells AFTER the marker so the row can be
 * emitted as an `<li>`. A marker with no content after it is not a bullet.
 */
function detectBullet(cells: VisualCell[]): {
  isBullet: boolean;
  contentCells: VisualCell[];
} {
  let i = 0;
  while (i < cells.length && !cells[i].code && cells[i].ch === " ") i++;
  const markedAndSpaced =
    i + 1 < cells.length &&
    !cells[i].code &&
    BULLET_MARKER_RE.test(cells[i].ch) &&
    cells[i + 1].ch === " ";
  if (!markedAndSpaced) return { isBullet: false, contentCells: cells };
  let j = i + 1;
  while (j < cells.length && cells[j].ch === " ") j++;
  const content = cells.slice(j);
  if (content.length === 0) return { isBullet: false, contentCells: cells };
  return { isBullet: true, contentCells: content };
}

/** Column separators a TUI draws markdown tables with (ASCII + box-drawing). */
const TABLE_SEP_RE = /^[|│┃║]$/u;

/** A char that can only appear in a table rule/border row (box-drawing + ASCII
 *  rule punctuation + whitespace). U+2500–U+257F is the box-drawing block. */
function isRuleChar(ch: string): boolean {
  if (ch === " " || ch === "\t") return true;
  if (ch === "-" || ch === "=" || ch === "+" || ch === "_") return true;
  if (TABLE_SEP_RE.test(ch)) return true;
  const cp = ch.codePointAt(0) ?? 0;
  return cp >= 0x2500 && cp <= 0x257f;
}

/**
 * A "rule" row is a table border / header-separator: non-blank, but made up
 * entirely of box-drawing / `-=+|` rule characters (e.g. `├────┼────┤` or
 * `|---|---|`). These are dropped when a table is reconstructed.
 */
function isRuleLine(cells: VisualCell[]): boolean {
  let sawRule = false;
  for (const c of cells) {
    if (c.code) return false; // inline code is cell content, never a border
    if (!isRuleChar(c.ch)) return false;
    if (c.ch !== " " && c.ch !== "\t") sawRule = true;
  }
  return sawRule;
}

/**
 * A table needs a divider between its header and body. Top/bottom borders alone
 * are not enough: Mermaid's terminal rendering also uses box-drawing borders,
 * and treating a stack of diagram nodes as a table destroys the diagram when
 * copied. Accept the two table forms Claude draws:
 *
 * - Markdown/ASCII: `|---|---|` (or `+---+---+`)
 * - Box drawing: `├───┼───┤` / `╠═══╬═══╣`
 */
function isTableDividerLine(cells: VisualCell[]): boolean {
  if (!isRuleLine(cells)) return false;
  const text = cells.map((c) => c.ch).join("");
  const hasHorizontalRule = /[-=─━╌╍═]/u.test(text);
  if (!hasHorizontalRule) return false;

  // Left/right/cross junctions identify a divider through an existing grid.
  // Plain ASCII/GFM dividers use `|` or `+` for the same purpose.
  return /[|+├-┫┼-╋╞-╣╪-╬]/u.test(text);
}

/**
 * A `|`/`│`/… cell is a column separator only when it is NOT inline code: the
 * table borders Claude's TUI draws are default-coloured, whereas a pipe inside
 * inline code (e.g. `` `a | b` ``) carries the code colour and is cell content.
 */
function isSeparatorCell(c: VisualCell): boolean {
  return !c.code && TABLE_SEP_RE.test(c.ch);
}

/** Count the column-separator cells in a row (`|`/`│`/…). */
function countSeparators(cells: VisualCell[]): number {
  let n = 0;
  for (const c of cells) if (isSeparatorCell(c)) n++;
  return n;
}

/** Drop leading/trailing non-code whitespace cells (per-table-cell trim). */
function trimBothWhitespace(cells: VisualCell[]): VisualCell[] {
  let s = 0;
  let e = cells.length;
  while (s < e && !cells[s].code && cells[s].ch === " ") s++;
  while (e > s && !cells[e - 1].code && cells[e - 1].ch === " ") e--;
  return cells.slice(s, e);
}

/** Split one content row into rendered cells, dropping the outer-border empties. */
function splitTableRow(
  cells: VisualCell[],
  org?: string,
): { text: string; html: string }[] {
  const groups: VisualCell[][] = [[]];
  for (const c of cells) {
    if (isSeparatorCell(c)) groups.push([]);
    else groups[groups.length - 1].push(c);
  }
  const rendered = groups.map((g) => renderLine(trimBothWhitespace(g), org));
  // The cells outside the first/last separator are border artifacts when empty.
  if (rendered.length > 1 && rendered[0].text === "") rendered.shift();
  if (rendered.length > 1 && rendered[rendered.length - 1].text === "")
    rendered.pop();
  return rendered;
}

type TableCell = { text: string; html: string };

/**
 * Merge the physical rows of one logical table row, column by column — this is
 * how a cell that wrapped onto extra physical lines (with the other columns left
 * blank on the continuation lines) is stitched back into a single cell. Empty
 * pieces are skipped; non-empty pieces are joined with a space.
 */
function mergeRowCells(rows: TableCell[][]): TableCell[] {
  const cols = Math.max(0, ...rows.map((r) => r.length));
  const out: TableCell[] = [];
  for (let k = 0; k < cols; k++) {
    const texts: string[] = [];
    const htmls: string[] = [];
    for (const r of rows) {
      const c = r[k];
      if (c && c.text !== "") {
        texts.push(c.text);
        htmls.push(c.html);
      }
    }
    out.push({ text: texts.join(" "), html: htmls.join(" ") });
  }
  return out;
}

/**
 * Reconstruct a detected table block into a real table: a GFM pipe table in
 * `text/plain` and an HTML `<table>` (first logical row → `<th>`/`<thead>`).
 * Inline code and PR links inside cells survive.
 *
 * Rule/border rows are dropped. When the block has row-rules (Claude's TUI draws
 * a horizontal rule between every logical row), those rules delimit logical rows
 * and the physical rows within each are merged — so a cell that wrapped across
 * lines becomes one cell again. A plain markdown pipe table (just the header
 * `---` separator) has no row-rules, so each content row stays its own row.
 */
function renderTable(
  rows: VisualCell[][],
  org?: string,
): { text: string; html: string } {
  // Split into rule-delimited segments; each segment is one logical row's worth
  // of physical lines.
  const segments: VisualCell[][][] = [];
  let current: VisualCell[][] = [];
  let ruleCount = 0;
  for (const r of rows) {
    if (isRuleLine(r)) {
      ruleCount++;
      if (current.length) {
        segments.push(current);
        current = [];
      }
    } else {
      current.push(r);
    }
  }
  if (current.length) segments.push(current);

  // ≥2 rules ⇒ row-ruled table: merge each segment's wrapped lines into one
  // logical row. Otherwise (markdown-style) every content row is its own row.
  const logical: TableCell[][] =
    ruleCount >= 2
      ? segments.map((seg) =>
          mergeRowCells(seg.map((r) => splitTableRow(r, org))),
        )
      : segments.flat().map((r) => splitTableRow(r, org));

  const cols = Math.max(0, ...logical.map((r) => r.length));
  for (const r of logical)
    while (r.length < cols) r.push({ text: "", html: "" });

  const [header = [], ...body] = logical;
  const thead = `<thead><tr>${header
    .map((c) => `<th>${c.html}</th>`)
    .join("")}</tr></thead>`;
  const tbody = body.length
    ? `<tbody>${body
        .map((r) => `<tr>${r.map((c) => `<td>${c.html}</td>`).join("")}</tr>`)
        .join("")}</tbody>`
    : "";
  const html = `<table>${thead}${tbody}</table>`;

  const toRow = (r: { text: string }[]) =>
    `| ${r.map((c) => c.text).join(" | ")} |`;
  const text = [
    toRow(header),
    `| ${Array(cols).fill("---").join(" | ")} |`,
    ...body.map(toRow),
  ].join("\n");
  return { text, html };
}

/** One assembled chunk: a reconstructed table, or a single prose/bullet line. */
type Unit =
  | { kind: "table"; text: string; html: string }
  | { kind: "line"; text: string; innerHtml: string; isBullet: boolean };

/**
 * Assemble units into the final document: a `table` unit emits its own HTML;
 * runs of consecutive bullet lines become one `<ul>` (Slack renders pasted
 * `<ul><li>` as a native list); other lines are joined with `<br>`.
 */
function assembleHtml(units: Unit[]): string {
  const isBulletLine = (u: Unit) => u.kind === "line" && u.isBullet;
  const isProseLine = (u: Unit) => u.kind === "line" && !u.isBullet;

  const parts: string[] = [];
  let i = 0;
  while (i < units.length) {
    const u = units[i];
    if (u.kind === "table") {
      parts.push(u.html);
      i++;
    } else if (u.isBullet) {
      const items: string[] = [];
      while (i < units.length && isBulletLine(units[i])) {
        items.push(`<li>${(units[i] as { innerHtml: string }).innerHtml}</li>`);
        i++;
      }
      parts.push(`<ul>${items.join("")}</ul>`);
    } else {
      const run: string[] = [];
      while (i < units.length && isProseLine(units[i])) {
        run.push((units[i] as { innerHtml: string }).innerHtml);
        i++;
      }
      parts.push(run.join("<br>"));
    }
  }
  return parts.join("");
}

/**
 * Group the logical lines into {@link Unit}s, reconstructing table blocks. A
 * table block is a maximal run of rows that are each a column-separated content
 * row (≥2 separators) or a rule/border row, and that contains ≥2 content rows
 * (so a stray pipe in prose is never mistaken for a table).
 */
function toUnits(logical: VisualCell[][], org?: string): Unit[] {
  const trimmed = logical.map(trimTrailing);
  const meta = trimmed.map((cells) => ({
    cells,
    rule: isRuleLine(cells),
    divider: isTableDividerLine(cells),
    seps: countSeparators(cells),
  }));

  const units: Unit[] = [];
  let i = 0;
  while (i < meta.length) {
    const m = meta[i];
    if (m.rule || m.seps >= 2) {
      let j = i;
      let content = 0;
      let sawContent = false;
      let dividerAfterContent = false;
      let contentAfterDivider = false;
      while (j < meta.length && (meta[j].rule || meta[j].seps >= 2)) {
        if (!meta[j].rule && meta[j].seps >= 2) {
          content++;
          sawContent = true;
          if (dividerAfterContent) contentAfterDivider = true;
        } else if (meta[j].divider && sawContent) {
          dividerAfterContent = true;
        }
        j++;
      }
      // Requiring a real header/body divider avoids the Mermaid false-positive:
      // diagram node borders are rules too, but they do not divide two rows in
      // one grid. Both GFM and box-drawn tables have this boundary.
      if (content >= 2 && dividerAfterContent && contentAfterDivider) {
        units.push({
          kind: "table",
          ...renderTable(
            meta.slice(i, j).map((x) => x.cells),
            org,
          ),
        });
        i = j;
        continue;
      }
    }
    const full = renderLine(m.cells, org);
    const { isBullet, contentCells } = detectBullet(m.cells);
    units.push({
      kind: "line",
      text: full.text,
      innerHtml: isBullet ? renderLine(contentCells, org).html : full.html,
      isBullet,
    });
    i++;
  }
  return units;
}

/** Extra knobs for {@link serializeTermSelection}. */
export interface TermCopyOptions {
  /**
   * Default GitHub org used to resolve the bare `repo#n` PR shorthand into a
   * link (e.g. `example-deploy#1155`). `owner/repo#n` and full URLs need no
   * org; when this is unset, `repo#n` is left as plain text.
   */
  org?: string;
}

/**
 * Build `{ text, html }` for the current selection, or `null` if there is no
 * selection (or it is empty after trimming). `text` is the Slack/markdown
 * plain-text flavour; `html` is the rich flavour Slack's composer consumes —
 * inline-code runs become `<code>`, GitHub PR references become `<a>` links,
 * `- `/`* `/`• ` rows become a native `<ul>` list, and box-drawing tables are
 * reconstructed (GFM pipe table in text, `<table>` in HTML).
 */
export function serializeTermSelection(
  term: TermLike,
  cfg: CodeColorConfig = resolveCodeColors(),
  opts: TermCopyOptions = {},
): { text: string; html: string } | null {
  const logical = collectLogicalLines(term, cfg);
  if (!logical) return null;

  const units = toUnits(logical, opts.org);
  if (units.length === 0) return null;
  if (units.every((u) => u.text.length === 0)) return null;

  return {
    text: units.map((u) => u.text).join("\n"),
    html: assembleHtml(units),
  };
}

export interface ColorSignature {
  /** `default` | `palette:N` | `rgb:#rrggbb`. */
  sig: string;
  /** Up to a few sample characters seen with this colour. */
  sample: string;
  count: number;
}

/**
 * Tally the distinct foreground-colour signatures over the current selection,
 * with a few sample characters each. This is the one-time DISCOVERY tool:
 * enable `localStorage.arrakisTermClipboardDebug = "1"`, select a span of blue
 * inline code in a live terminal, copy, and read the logged table — the
 * signature dominating the code text is the one to add to
 * `localStorage.arrakisTermCodeColors` (or {@link DEFAULT_CODE_COLORS}).
 */
export function selectionColorSignatures(term: TermLike): ColorSignature[] {
  const range = term.getSelectionPosition();
  if (!range) return [];
  const acc = new Map<string, { sample: string; count: number }>();
  for (let y = range.start.y; y <= range.end.y; y++) {
    const line = term.buffer.active.getLine(y);
    if (!line) continue;
    const colStart = y === range.start.y ? range.start.x : 0;
    const colEnd = y === range.end.y ? range.end.x : line.length;
    for (let x = colStart; x < colEnd; x++) {
      const cell = line.getCell(x);
      if (!cell || cell.getWidth() === 0) continue;
      const ch = cell.getChars();
      if (ch === "" || /^\s$/.test(ch)) continue; // colour of blanks is noise
      const sig = cell.isFgDefault()
        ? "default"
        : cell.isFgPalette()
          ? `palette:${cell.getFgColor()}`
          : cell.isFgRGB()
            ? `rgb:#${cell.getFgColor().toString(16).padStart(6, "0")}`
            : `mode?:${cell.getFgColor()}`;
      const entry = acc.get(sig) ?? { sample: "", count: 0 };
      entry.count++;
      if (entry.sample.length < 24) entry.sample += ch;
      acc.set(sig, entry);
    }
  }
  return [...acc.entries()]
    .map(([sig, v]) => ({ sig, sample: v.sample, count: v.count }))
    .sort((a, b) => b.count - a.count);
}

/** Whether the discovery debug log is enabled via localStorage. */
export function isClipboardDebugEnabled(
  storage: Pick<Storage, "getItem"> | undefined = safeLocalStorage(),
): boolean {
  try {
    return storage?.getItem(LS_DEBUG_KEY) === "1";
  } catch {
    return false;
  }
}
