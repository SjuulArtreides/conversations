import "fake-indexeddb/auto";
import {
  clearTerminalCache,
  loadTerminalSnapshot,
  saveTerminalSnapshot,
} from "./terminalCache";
import { createTerminalReadingView } from "./terminalReadingView";
import { afterEach, expect, test, vi } from "vitest";
import { Terminal, type ILink, type ILinkProvider } from "@xterm/xterm";
import { SerializeAddon } from "@xterm/addon-serialize";
import { createLinkClickFilter, createLinkedTerminal } from "./linkedTerminal";

const terminals: Terminal[] = [];
afterEach(async () => {
  await clearTerminalCache();
  terminals.forEach((term) => term.dispose());
  terminals.length = 0;
  vi.restoreAllMocks();
});
const write = (term: Terminal, bytes: string) =>
  new Promise<void>((resolve) => term.write(bytes, resolve));

function setup(cols = 80) {
  const providers: ILinkProvider[] = [];
  const register = Terminal.prototype.registerLinkProvider;
  const spy = vi
    .spyOn(Terminal.prototype, "registerLinkProvider")
    .mockImplementation(function (this: Terminal, provider) {
      providers.push(provider);
      return register.call(this, provider);
    });
  const term = createLinkedTerminal({ cols, rows: 6, allowProposedApi: true });
  spy.mockRestore();
  terminals.push(term);
  return {
    term,
    async links(row: number) {
      const groups = await Promise.all(
        providers.map(
          (provider) =>
            new Promise<ILink[]>((resolve) =>
              provider.provideLinks(row, (links) => resolve(links ?? [])),
            ),
        ),
      );
      return groups.flat();
    },
  };
}

const url = "https://linear.app/example/issue/ENG-2422";

test("plain URLs are clickable after live output and snapshot redraw", async () => {
  const { term, links } = setup();
  const popup = { opener: window, location: { href: "" } };
  vi.spyOn(window, "open").mockReturnValue(popup as unknown as Window);
  for (const bytes of [url, `\x1b[H\x1b[2J${url}`]) {
    await write(term, bytes);
    const found = await links(1);
    expect(found.map((link) => link.text)).toEqual([url]);
    found[0].activate(new MouseEvent("click"), found[0].text);
    expect(popup.location.href).toBe(url);
    expect(popup.opener).toBeNull();
  }
});

test("OSC 8 links ask before navigating; plain URLs do not", async () => {
  const { term, links } = setup();
  const target = "https://example.test/target";
  const popup = { opener: window, location: { href: "" } };
  const open = vi
    .spyOn(window, "open")
    .mockReturnValue(popup as unknown as Window);
  const confirmSpy = vi.spyOn(window, "confirm");
  const range = {} as never;
  const { linkHandler } = term.options;

  confirmSpy.mockReturnValue(false);
  linkHandler!.activate(new MouseEvent("click"), target, range);
  expect(confirmSpy).toHaveBeenCalledWith(expect.stringContaining(target));
  expect(open).not.toHaveBeenCalled();

  confirmSpy.mockReturnValue(true);
  linkHandler!.activate(new MouseEvent("click"), target, range);
  expect(open).toHaveBeenCalledTimes(1);
  expect(popup.location.href).toBe(target);
  expect(popup.opener).toBeNull();

  // Warns and navigates nowhere when the popup is blocked.
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  open.mockReturnValue(null);
  linkHandler!.activate(new MouseEvent("click"), target, range);
  expect(warn).toHaveBeenCalledTimes(1);

  await write(term, url);
  confirmSpy.mockClear();
  open.mockReturnValue(popup as unknown as Window);
  const [plain] = await links(1);
  plain.activate(new MouseEvent("click"), plain.text);
  expect(confirmSpy).not.toHaveBeenCalled();
});

test("a click that starts on a link is not forwarded as a mouse report", async () => {
  const { term, links } = setup();
  await write(term, url);
  const [link] = await links(1);
  const forward = createLinkClickFilter(term);
  const press = "\x1b[<0;3;1M";
  const release = "\x1b[<0;3;1m";
  const move = new MouseEvent("mousemove");

  link.hover?.(move, link.text);
  expect([forward(press), forward(release)]).toEqual([false, false]);
  expect(forward("\x1b[<64;3;1M")).toBe(true);

  link.leave?.(move, link.text);
  expect([forward(press), forward(release)]).toEqual([true, true]);
});

test("wrapped URLs remain links after serialization into the reading view", async () => {
  const live = setup(24);
  const serialize = new SerializeAddon();
  live.term.loadAddon(serialize);
  await write(live.term, url);
  const reader = setup(24);
  await write(reader.term, serialize.serialize());
  for (const terminal of [live, reader]) {
    for (const row of [1, 2]) {
      const found = await terminal.links(row);
      expect(found.map((link) => link.text)).toEqual([url]);
      expect(found[0].range).toEqual({
        start: { x: 1, y: 1 },
        end: { x: 17, y: 2 },
      });
    }
  }
});

const labelledLink = (target: string, label = "PR") =>
  `\x1b]8;;${target}\x1b\\${label}\x1b]8;;\x1b\\`;

test("labelled links retain distinct targets through serialized replay", async () => {
  const live = setup();
  const serialize = new SerializeAddon();
  live.term.loadAddon(serialize);
  const targets = [
    "https://github.com/example/example/pull/1",
    "https://github.com/example/example/pull/2",
  ];
  await write(
    live.term,
    targets.map((target) => labelledLink(target)).join(" "),
  );
  const bytes = serialize.serialize();
  expect(bytes).toContain(`;${targets[0]}\x1b\\`);
  expect(bytes).toContain(`;${targets[1]}\x1b\\`);
  await saveTerminalSnapshot(
    "osc8-cache",
    bytes,
    live.term.cols,
    live.term.rows,
  );
  const cached = await loadTerminalSnapshot("osc8-cache");
  expect(cached).not.toBeNull();
  const reader = setup();
  const replayed: string[] = [];
  reader.term.parser.registerOscHandler(8, (data) => {
    const uri = data.slice(data.indexOf(";") + 1);
    if (uri) replayed.push(uri);
    return false;
  });
  await write(reader.term, cached!.serialized);
  expect(reader.term.buffer.active.getLine(0)?.translateToString(true)).toBe(
    "PR PR",
  );
  expect(replayed).toEqual(targets);
  expect(targetAt(reader.term, 0, 0)).toBe(targets[0]);
  expect(targetAt(reader.term, 0, 2)).toBeUndefined();
  expect(targetAt(reader.term, 0, 3)).toBe(targets[1]);
});

// xterm has no public hyperlink-cell inspection API. Match its native provider's
// read-only access, so these tests verify target ownership after actual parsing.
function targetAt(term: Terminal, row: number, col: number) {
  const cell = term.buffer.active.getLine(row)?.getCell(col) as
    { hasExtendedAttrs(): boolean; extended: { urlId: number } } | undefined;
  const core = term as unknown as {
    _core: {
      _oscLinkService: { getLinkData(id: number): { uri: string } | undefined };
    };
  };
  return cell?.hasExtendedAttrs()
    ? core._core._oscLinkService.getLinkData(cell.extended.urlId)?.uri
    : undefined;
}

test("wrapped labelled links and linked whitespace keep their exact targets", async () => {
  const live = setup(4);
  const serialize = new SerializeAddon();
  live.term.loadAddon(serialize);
  const target = "https://example.test/wrapped";
  await write(live.term, labelledLink(target, "ab cd ef") + "!");
  const reader = setup(4);
  await write(reader.term, serialize.serialize());
  for (const term of [live.term, reader.term]) {
    expect(targetAt(term, 0, 0)).toBe(target);
    expect(targetAt(term, 0, 2)).toBe(target);
    expect(targetAt(term, 1, 0)).toBe(target);
    expect(targetAt(term, 1, 3)).toBe(target);
    expect(targetAt(term, 2, 0)).toBeUndefined();
    expect(term.buffer.active.getLine(1)?.isWrapped).toBe(true);
  }
});

test.each([false, true])(
  "serialized cursor retains its actual active hyperlink: %s",
  async (active) => {
    const live = setup();
    const serialize = new SerializeAddon();
    live.term.loadAddon(serialize);
    const target = "https://example.test/cursor";
    await write(
      live.term,
      `\x1b]8;;${target}\x1b\\A${active ? "" : "\x1b]8;;\x1b\\"}`,
    );
    const reader = setup();
    await write(reader.term, serialize.serialize());
    await write(reader.term, "B");
    expect(targetAt(reader.term, 0, 0)).toBe(target);
    expect(targetAt(reader.term, 0, 1)).toBe(active ? target : undefined);
  },
);

test("alternate buffer replay preserves labelled targets without leaking to normal output", async () => {
  const live = setup();
  const serialize = new SerializeAddon();
  live.term.loadAddon(serialize);
  await write(
    live.term,
    labelledLink("https://example.test/normal") +
      "\x1b[?1049h\x1b[H" +
      labelledLink("https://example.test/alternate"),
  );
  const reader = setup();
  await write(reader.term, serialize.serialize());
  expect(targetAt(reader.term, 0, 0)).toBe("https://example.test/alternate");
  await write(reader.term, "\x1b[?1049l");
  expect(targetAt(reader.term, 0, 0)).toBe("https://example.test/normal");
  await write(reader.term, "plain");
  expect(targetAt(reader.term, 0, 2)).toBeUndefined();
});

test("frozen reading view retains labelled targets while live output redraws", async () => {
  const live = setup();
  const serializer = new SerializeAddon();
  live.term.loadAddon(serializer);
  await write(
    live.term,
    labelledLink("https://example.test/frozen") +
      "\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7",
  );
  live.term.scrollLines(-1);
  let shown = live.term;
  const view = createTerminalReadingView(
    live.term,
    () => serializer.serialize(),
    () => setup().term,
    (reader) => {
      shown = reader ?? live.term;
    },
  );
  await new Promise<void>((resolve) => view.beforeWrite(resolve));
  await write(live.term, "\x1b[H\x1b[2Jnew output");
  expect(shown).not.toBe(live.term);
  expect(targetAt(shown, 0, 0)).toBe("https://example.test/frozen");
  view.dispose();
});

test("style resets inside a labelled link preserve its target", async () => {
  const live = setup();
  const serialize = new SerializeAddon();
  live.term.loadAddon(serialize);
  const target = "https://example.test/styled";
  await write(live.term, labelledLink(target, "A\x1b[31mB\x1b[0mC"));
  const reader = setup();
  await write(reader.term, serialize.serialize());
  for (const col of [0, 1, 2]) {
    expect(targetAt(reader.term, 0, col)).toBe(target);
  }
});
