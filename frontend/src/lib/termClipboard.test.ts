import { describe, it, expect } from "vitest";
import {
  serializeTermSelection,
  selectionColorSignatures,
  resolveCodeColors,
  resolveGithubOrg,
  codeColorsFromDto,
  DEFAULT_CODE_COLORS,
  type TermLike,
  type CellLike,
  type LineLike,
  type CodeColorConfig,
} from "./termClipboard";

// ── Fake xterm buffer ───────────────────────────────────────────────────────
//
// A cell is described compactly as a [char, fg] tuple, where `fg` is:
//   undefined        → default foreground (not code)
//   { p: N }         → palette index N
//   { rgb: 0xRRGGBB }→ truecolor
type Fg = { p: number } | { rgb: number } | undefined;
type CellSpec = [string, Fg];

function cell([ch, fg]: CellSpec): CellLike {
  return {
    getChars: () => ch,
    getWidth: () => (ch === "" ? 1 : [...ch].length > 1 ? 2 : 1),
    getFgColor: () =>
      fg && "p" in fg ? fg.p : fg && "rgb" in fg ? fg.rgb : -1,
    isFgDefault: () => fg === undefined,
    isFgPalette: () => !!fg && "p" in fg,
    isFgRGB: () => !!fg && "rgb" in fg,
  };
}

function line(cells: CellSpec[], isWrapped = false): LineLike {
  const built = cells.map(cell);
  return {
    length: built.length,
    isWrapped,
    getCell: (x: number) => built[x],
  };
}

type Range = ReturnType<TermLike["getSelectionPosition"]>;

function term(lines: LineLike[], range: Range): TermLike {
  return {
    getSelectionPosition: () => range,
    buffer: { active: { getLine: (y: number) => lines[y] } },
  };
}

const CODE_PALETTE: CodeColorConfig = {
  palette: new Set([6]),
  rgb: new Set(),
};

function row(text: string, fg: Fg): CellSpec[] {
  return [...text].map((ch) => [ch, fg] as CellSpec);
}

describe("serializeTermSelection", () => {
  it("returns null when there is no selection", () => {
    const t = term([line(row("hello", undefined))], undefined);
    expect(serializeTermSelection(t, CODE_PALETTE)).toBeNull();
  });

  it("copies plain text unchanged when no cell matches a code colour", () => {
    const cells = row("plain text", undefined);
    const t = term([line(cells)], {
      start: { x: 0, y: 0 },
      end: { x: cells.length, y: 0 },
    });
    const out = serializeTermSelection(t, CODE_PALETTE);
    expect(out).toEqual({ text: "plain text", html: "plain text" });
  });

  it("wraps a palette-coloured run in backticks (text) and <code> (html)", () => {
    // "set active=f now" with "active=f" in palette 6.
    const cells: CellSpec[] = [
      ...row("set ", undefined),
      ...row("active=f", { p: 6 }),
      ...row(" now", undefined),
    ];
    const t = term([line(cells)], {
      start: { x: 0, y: 0 },
      end: { x: cells.length, y: 0 },
    });
    const out = serializeTermSelection(t, CODE_PALETTE)!;
    expect(out.text).toBe("set `active=f` now");
    expect(out.html).toBe("set <code>active=f</code> now");
  });

  it("matches truecolor (RGB) code colours", () => {
    const cfg: CodeColorConfig = {
      palette: new Set(),
      rgb: new Set([0x58a6ff]),
    };
    const cells: CellSpec[] = [
      ...row("x ", undefined),
      ...row("code", { rgb: 0x58a6ff }),
    ];
    const t = term([line(cells)], {
      start: { x: 0, y: 0 },
      end: { x: cells.length, y: 0 },
    });
    const out = serializeTermSelection(t, cfg)!;
    expect(out.text).toBe("x `code`");
    expect(out.html).toBe("x <code>code</code>");
  });

  it("escapes HTML in both code and non-code text", () => {
    const cells: CellSpec[] = [
      ...row("a < b && ", undefined),
      ...row("<T>", { p: 6 }),
    ];
    const t = term([line(cells)], {
      start: { x: 0, y: 0 },
      end: { x: cells.length, y: 0 },
    });
    const out = serializeTermSelection(t, CODE_PALETTE)!;
    expect(out.html).toBe("a &lt; b &amp;&amp; <code>&lt;T&gt;</code>");
    // Plain text keeps the raw characters.
    expect(out.text).toBe("a < b && `<T>`");
  });

  it("does not single-backtick a run that contains a backtick", () => {
    const cells = row("a`b", { p: 6 });
    const t = term([line(cells)], {
      start: { x: 0, y: 0 },
      end: { x: cells.length, y: 0 },
    });
    const out = serializeTermSelection(t, CODE_PALETTE)!;
    expect(out.text).toBe("a`b"); // not fenced
    expect(out.html).toBe("<code>a`b</code>"); // html still marks it
  });

  it("trims trailing whitespace but keeps it inside a code run", () => {
    const cells: CellSpec[] = [
      ...row("hi", undefined),
      ...row("  ", undefined), // trailing spaces → trimmed
    ];
    const t = term([line(cells)], {
      start: { x: 0, y: 0 },
      end: { x: cells.length, y: 0 },
    });
    expect(serializeTermSelection(t, CODE_PALETTE)!.text).toBe("hi");
  });

  it("joins separate rows with newline / <br>", () => {
    const t = term([line(row("one", undefined)), line(row("two", undefined))], {
      start: { x: 0, y: 0 },
      end: { x: 3, y: 1 },
    });
    const out = serializeTermSelection(t, CODE_PALETTE)!;
    expect(out.text).toBe("one\ntwo");
    expect(out.html).toBe("one<br>two");
  });

  it("merges soft-wrapped rows into one logical line and keeps a wrapped code run intact", () => {
    // "longcode" is one palette run split across a wrap boundary.
    const t = term(
      [line(row("long", { p: 6 })), line(row("code", { p: 6 }), true)],
      { start: { x: 0, y: 0 }, end: { x: 4, y: 1 } },
    );
    const out = serializeTermSelection(t, CODE_PALETTE)!;
    expect(out.text).toBe("`longcode`");
    expect(out.html).toBe("<code>longcode</code>");
  });

  it("honours the per-row selection columns (first/last row partial)", () => {
    // Select from x=2 on row0 to x=2 (exclusive) on row1.
    const t = term(
      [line(row("abcde", undefined)), line(row("fghij", undefined))],
      { start: { x: 2, y: 0 }, end: { x: 2, y: 1 } },
    );
    const out = serializeTermSelection(t, CODE_PALETTE)!;
    expect(out.text).toBe("cde\nfg");
  });
});

describe("serializeTermSelection — GitHub PR references", () => {
  function single(text: string, org?: string) {
    const cells = row(text, undefined);
    const t = term([line(cells)], {
      start: { x: 0, y: 0 },
      end: { x: cells.length, y: 0 },
    });
    return serializeTermSelection(t, CODE_PALETTE, { org })!;
  }

  it("links an owner/repo#n shorthand with no org configured", () => {
    const out = single("see owner/repo#12 now");
    expect(out.html).toBe(
      'see <a href="https://github.com/owner/repo/pull/12">owner/repo#12</a> now',
    );
    expect(out.text).toBe("see owner/repo#12 now"); // plain text left as typed
  });

  it("links a full github.com PR/issue URL", () => {
    const out = single("x https://github.com/o/r/pull/5 y");
    expect(out.html).toBe(
      'x <a href="https://github.com/o/r/pull/5">https://github.com/o/r/pull/5</a> y',
    );
  });

  it("links a bare repo#n shorthand only when an org is supplied", () => {
    const withOrg = single("ship example-deploy#1155", "acme");
    expect(withOrg.html).toBe(
      'ship <a href="https://github.com/acme/example-deploy/pull/1155">example-deploy#1155</a>',
    );
    const noOrg = single("ship example-deploy#1155");
    expect(noOrg.html).toBe("ship example-deploy#1155"); // stays plain text
  });

  it("does not linkify inside a code run", () => {
    const cells: CellSpec[] = [
      ...row("x ", undefined),
      ...row("a/b#1", { p: 6 }),
    ];
    const t = term([line(cells)], {
      start: { x: 0, y: 0 },
      end: { x: cells.length, y: 0 },
    });
    const out = serializeTermSelection(t, CODE_PALETTE, { org: "acme" })!;
    expect(out.html).toBe("x <code>a/b#1</code>");
  });
});

describe("serializeTermSelection — bullet lists", () => {
  it("turns `- ` rows into a native <ul> list (Slack renders it)", () => {
    const t = term(
      [
        line(row("- one", undefined)),
        line(row("- two", undefined)),
        line(row("- three", undefined)),
      ],
      { start: { x: 0, y: 0 }, end: { x: 7, y: 2 } },
    );
    const out = serializeTermSelection(t, CODE_PALETTE)!;
    expect(out.html).toBe("<ul><li>one</li><li>two</li><li>three</li></ul>");
    expect(out.text).toBe("- one\n- two\n- three"); // plain keeps the markers
  });

  it("groups only consecutive bullet rows, keeping prose as <br> lines", () => {
    const t = term(
      [
        line(row("Summary:", undefined)),
        line(row("- a", undefined)),
        line(row("- b", undefined)),
        line(row("done", undefined)),
      ],
      { start: { x: 0, y: 0 }, end: { x: 4, y: 3 } },
    );
    const out = serializeTermSelection(t, CODE_PALETTE)!;
    expect(out.html).toBe("Summary:<ul><li>a</li><li>b</li></ul>done");
  });

  it("linkifies and keeps code inside a bullet item", () => {
    const cells: CellSpec[] = [
      ...row("- bump ", undefined),
      ...row("v0.4.0", { p: 6 }),
      ...row(" in owner/repo#7", undefined),
    ];
    const t = term([line(cells)], {
      start: { x: 0, y: 0 },
      end: { x: cells.length, y: 0 },
    });
    const out = serializeTermSelection(t, CODE_PALETTE)!;
    expect(out.html).toBe(
      '<ul><li>bump <code>v0.4.0</code> in <a href="https://github.com/owner/repo/pull/7">owner/repo#7</a></li></ul>',
    );
  });

  it("does not treat a mid-line dash (e.g. `set -x`) as a bullet", () => {
    const cells = row("set -x flag", undefined);
    const t = term([line(cells)], {
      start: { x: 0, y: 0 },
      end: { x: cells.length, y: 0 },
    });
    const out = serializeTermSelection(t, CODE_PALETTE)!;
    expect(out.html).toBe("set -x flag");
  });
});

describe("serializeTermSelection — tables", () => {
  function tableTerm(rows: CellSpec[][]) {
    const lines = rows.map((r) => line(r));
    const last = rows[rows.length - 1].length;
    return term(lines, {
      start: { x: 0, y: 0 },
      end: { x: last, y: rows.length - 1 },
    });
  }

  it("reconstructs an ASCII-pipe table into GFM text + an HTML <table>", () => {
    const out = serializeTermSelection(
      tableTerm([
        row("| Tool | Fit |", undefined),
        row("|------|-----|", undefined),
        row("| Firehose | Low |", undefined),
        row("| Lambda | Low |", undefined),
      ]),
      CODE_PALETTE,
    )!;
    expect(out.text).toBe(
      "| Tool | Fit |\n| --- | --- |\n| Firehose | Low |\n| Lambda | Low |",
    );
    expect(out.html).toBe(
      "<table><thead><tr><th>Tool</th><th>Fit</th></tr></thead>" +
        "<tbody><tr><td>Firehose</td><td>Low</td></tr>" +
        "<tr><td>Lambda</td><td>Low</td></tr></tbody></table>",
    );
  });

  it("reconstructs a box-drawing (│ ─ ┼) table, dropping border rows", () => {
    const out = serializeTermSelection(
      tableTerm([
        row("┌──────┬─────┐", undefined),
        row("│ Tool │ Fit │", undefined),
        row("├──────┼─────┤", undefined),
        row("│ A    │ B   │", undefined),
        row("└──────┴─────┘", undefined),
      ]),
      CODE_PALETTE,
    )!;
    expect(out.text).toBe("| Tool | Fit |\n| --- | --- |\n| A | B |");
    expect(out.html).toBe(
      "<table><thead><tr><th>Tool</th><th>Fit</th></tr></thead>" +
        "<tbody><tr><td>A</td><td>B</td></tr></tbody></table>",
    );
  });

  it("reconstructs a double-line box-drawing table", () => {
    const out = serializeTermSelection(
      tableTerm([
        row("╔═══╦═══╗", undefined),
        row("║ H ║ V ║", undefined),
        row("╠═══╬═══╣", undefined),
        row("║ A ║ B ║", undefined),
        row("╚═══╩═══╝", undefined),
      ]),
      CODE_PALETTE,
    )!;
    expect(out.text).toBe("| H | V |\n| --- | --- |\n| A | B |");
    expect(out.html).toContain("<table>");
  });

  it("does not treat vertical dashed box characters as a table divider", () => {
    const lines = [
      "║ Header ║ Value ║",
      "╞╎╎╎╪╏╏╏╡",
      "║ A      ║ B     ║",
    ];
    const out = serializeTermSelection(
      tableTerm(lines.map((text) => row(text, undefined))),
      CODE_PALETTE,
    )!;
    expect(out.text).toBe(lines.join("\n"));
    expect(out.html).not.toContain("<table>");
  });

  it("keeps a box-drawn Mermaid-style flowchart as a diagram", () => {
    const out = serializeTermSelection(
      tableTerm([
        row("┌─────────┐", undefined),
        row("│ Browser │", undefined),
        row("└────┬────┘", undefined),
        row("     │", undefined),
        row("     ▼", undefined),
        row("┌────┴────┐", undefined),
        row("│ Server  │", undefined),
        row("└─────────┘", undefined),
      ]),
      CODE_PALETTE,
    )!;
    expect(out.text).toBe(
      [
        "┌─────────┐",
        "│ Browser │",
        "└────┬────┘",
        "     │",
        "     ▼",
        "┌────┴────┐",
        "│ Server  │",
        "└─────────┘",
      ].join("\n"),
    );
    expect(out.html).not.toContain("<table>");
  });

  it("keeps inline code and PR links inside table cells", () => {
    const out = serializeTermSelection(
      tableTerm([
        row("| File | PR |", undefined),
        row("|---|---|", undefined),
        [
          ...row("| ", undefined),
          ...row(".jsonl.gz", { p: 6 }),
          ...row(" | owner/repo#7 |", undefined),
        ],
      ]),
      CODE_PALETTE,
    )!;
    expect(out.html).toContain("<td><code>.jsonl.gz</code></td>");
    expect(out.html).toContain(
      '<td><a href="https://github.com/owner/repo/pull/7">owner/repo#7</a></td>',
    );
    expect(out.text).toContain("| `.jsonl.gz` | owner/repo#7 |");
  });

  it("keeps prose before and after a table block", () => {
    const out = serializeTermSelection(
      tableTerm([
        row("Options:", undefined),
        row("| A | B |", undefined),
        row("|---|---|", undefined),
        row("| 1 | 2 |", undefined),
        row("done", undefined),
      ]),
      CODE_PALETTE,
    )!;
    expect(out.html).toBe(
      "Options:<table><thead><tr><th>A</th><th>B</th></tr></thead>" +
        "<tbody><tr><td>1</td><td>2</td></tr></tbody></table>done",
    );
    expect(out.text).toBe(
      "Options:\n| A | B |\n| --- | --- |\n| 1 | 2 |\ndone",
    );
  });

  it("merges wrapped cell continuations between row rules", () => {
    // Claude's TUI draws a rule between every logical row; a long cell wraps
    // onto a continuation line (other columns blank) with NO rule before it.
    const out = serializeTermSelection(
      tableTerm([
        row("┌───┬───┬───┐", undefined),
        row("| Comment | Severity | Fix |", undefined),
        row("├───┼───┼───┤", undefined),
        row("| Telemetry | Major | Added foo |", undefined),
        row("|  |  | before exit |", undefined), // wrapped continuation of col 3
        row("├───┼───┼───┤", undefined),
        row("| Global parallel | Minor | asserts |", undefined),
        row("| test |  |  |", undefined), // wrapped continuation of col 1
        row("└───┴───┴───┘", undefined),
      ]),
      CODE_PALETTE,
    )!;
    expect(out.text).toBe(
      "| Comment | Severity | Fix |\n" +
        "| --- | --- | --- |\n" +
        "| Telemetry | Major | Added foo before exit |\n" +
        "| Global parallel test | Minor | asserts |",
    );
    expect(out.html).toBe(
      "<table><thead><tr><th>Comment</th><th>Severity</th><th>Fix</th></tr></thead>" +
        "<tbody>" +
        "<tr><td>Telemetry</td><td>Major</td><td>Added foo before exit</td></tr>" +
        "<tr><td>Global parallel test</td><td>Minor</td><td>asserts</td></tr>" +
        "</tbody></table>",
    );
  });

  it("treats pipes inside inline code as cell content, not separators", () => {
    // The Notes cell holds inline code that itself contains pipe characters —
    // those are code-coloured, so they must NOT split the row into columns.
    const out = serializeTermSelection(
      tableTerm([
        row("| Col | Note |", undefined),
        row("|---|---|", undefined),
        [
          ...row("| x | ", undefined),
          ...row("| a | b |", { p: 6 }), // inline code with literal pipes
          ...row(" |", undefined),
        ],
      ]),
      CODE_PALETTE,
    )!;
    expect(out.html).toBe(
      "<table><thead><tr><th>Col</th><th>Note</th></tr></thead>" +
        "<tbody><tr><td>x</td><td><code>| a | b |</code></td></tr></tbody></table>",
    );
    expect(out.text).toBe("| Col | Note |\n| --- | --- |\n| x | `| a | b |` |");
  });

  it("does not treat a lone pipe-containing prose line as a table", () => {
    const cells = row("a | b | c", undefined);
    const t = term([line(cells)], {
      start: { x: 0, y: 0 },
      end: { x: cells.length, y: 0 },
    });
    const out = serializeTermSelection(t, CODE_PALETTE)!;
    expect(out.html).toBe("a | b | c");
    expect(out.text).toBe("a | b | c");
  });
});

describe("resolveGithubOrg", () => {
  it("returns the inferred org when there is no override", () => {
    expect(resolveGithubOrg({ getItem: () => null }, "acme")).toBe("acme");
  });

  it("prefers the localStorage override over the inferred org", () => {
    expect(resolveGithubOrg({ getItem: () => "manual" }, "acme")).toBe(
      "manual",
    );
  });

  it("returns undefined when neither is set", () => {
    expect(
      resolveGithubOrg({ getItem: () => null }, undefined),
    ).toBeUndefined();
    expect(resolveGithubOrg({ getItem: () => "   " }, "  ")).toBeUndefined();
  });
});

describe("selectionColorSignatures", () => {
  it("tallies distinct fg signatures with samples, ignoring blanks", () => {
    const cells: CellSpec[] = [
      ...row("ab", undefined),
      ...row(" ", undefined), // blank → ignored
      ...row("XY", { p: 6 }),
      ...row("Z", { rgb: 0x58a6ff }),
    ];
    const t = term([line(cells)], {
      start: { x: 0, y: 0 },
      end: { x: cells.length, y: 0 },
    });
    const sigs = selectionColorSignatures(t);
    const byKey = Object.fromEntries(sigs.map((s) => [s.sig, s]));
    expect(byKey["default"].sample).toBe("ab");
    expect(byKey["palette:6"]).toMatchObject({ sample: "XY", count: 2 });
    expect(byKey["rgb:#58a6ff"]).toMatchObject({ sample: "Z", count: 1 });
  });
});

describe("resolveCodeColors", () => {
  it("falls back to defaults with no override", () => {
    expect(resolveCodeColors({ getItem: () => null })).toBe(
      DEFAULT_CODE_COLORS,
    );
  });

  it("parses a JSON override with numeric and hex-string entries", () => {
    const cfg = resolveCodeColors({
      getItem: () =>
        JSON.stringify({ palette: [6, 14], rgb: ["#58a6ff", "abcdef"] }),
    });
    expect([...cfg.palette].sort((a, b) => a - b)).toEqual([6, 14]);
    expect(cfg.rgb.has(0x58a6ff)).toBe(true);
    expect(cfg.rgb.has(0xabcdef)).toBe(true);
  });

  it("falls back to defaults on malformed JSON", () => {
    expect(resolveCodeColors({ getItem: () => "{not json" })).toBe(
      DEFAULT_CODE_COLORS,
    );
  });

  it("returns the provided fallback (server setting) when no override", () => {
    const serverCfg: CodeColorConfig = {
      palette: new Set([3]),
      rgb: new Set([0x010203]),
    };
    expect(resolveCodeColors({ getItem: () => null }, serverCfg)).toBe(
      serverCfg,
    );
  });

  it("localStorage override wins over the fallback", () => {
    const serverCfg: CodeColorConfig = {
      palette: new Set([3]),
      rgb: new Set(),
    };
    const cfg = resolveCodeColors(
      { getItem: () => JSON.stringify({ rgb: [0x58a6ff] }) },
      serverCfg,
    );
    expect(cfg.rgb.has(0x58a6ff)).toBe(true);
    expect(cfg.palette.has(3)).toBe(false); // fallback NOT merged — override replaces
  });
});

describe("codeColorsFromDto", () => {
  it("returns null for null/undefined", () => {
    expect(codeColorsFromDto(null)).toBeNull();
    expect(codeColorsFromDto(undefined)).toBeNull();
  });

  it("converts a server DTO's palette/rgb arrays into sets", () => {
    const cfg = codeColorsFromDto({ palette: [6], rgb: [0xb1b9f9] })!;
    expect(cfg.palette.has(6)).toBe(true);
    expect(cfg.rgb.has(0xb1b9f9)).toBe(true);
  });

  it("tolerates missing arrays", () => {
    const cfg = codeColorsFromDto({ rgb: [0xb1b9f9] })!;
    expect(cfg.palette.size).toBe(0);
    expect(cfg.rgb.has(0xb1b9f9)).toBe(true);
  });
});

describe("DEFAULT_CODE_COLORS (Claude Code inline-code colour)", () => {
  it("wraps a #b1b9f9 run out of the box (no override needed)", () => {
    const cells: CellSpec[] = [
      ...row("run ", undefined),
      ...row("release.yml", { rgb: 0xb1b9f9 }),
    ];
    const t = term([line(cells)], {
      start: { x: 0, y: 0 },
      end: { x: cells.length, y: 0 },
    });
    const out = serializeTermSelection(t, DEFAULT_CODE_COLORS)!;
    expect(out.text).toBe("run `release.yml`");
    expect(out.html).toBe("run <code>release.yml</code>");
  });
});
