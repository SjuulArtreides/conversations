import { test, onTestFinished, expect } from "vitest";
import { Terminal } from "@xterm/xterm";
import { SerializeAddon } from "@xterm/addon-serialize";
import { createTerminalReadingView } from "./terminalReadingView";
import {
  captureTerminalViewport,
  restoreTerminalViewport,
} from "./terminalViewport";
const write = (term: Terminal, bytes: string) =>
  new Promise<void>((resolve) => term.write(bytes, resolve));
function viewport(term: Terminal) {
  const b = term.buffer.active;
  return Array.from({ length: term.rows }, (_, i) =>
    b.getLine(b.viewportY + i)?.translateToString(true),
  );
}
function setup() {
  const options = {
    cols: 80,
    rows: 6,
    scrollback: 5000,
    scrollOnEraseInDisplay: true,
  };
  const live = new Terminal(options);
  const serializer = new SerializeAddon();
  live.loadAddon(serializer);
  let shown = live;
  const view = createTerminalReadingView(
    live,
    () => serializer.serialize(),
    () => new Terminal(options),
    (reader) => {
      shown = reader || live;
    },
  );
  onTestFinished(() => {
    view.dispose();
    live.dispose();
  });
  return { live, view, shown: () => shown };
}

test("reading view survives a cursor update below an unchanged anchor row", async () => {
  const { live, view, shown } = setup();
  await write(
    live,
    "one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven\r\neight",
  );
  live.scrollLines(-1);
  const before = viewport(live);
  await new Promise<void>((resolve) => view.beforeWrite(resolve));
  const anchor = captureTerminalViewport(live);
  await write(live, "\x1b[2;1H\x1b[2KCHANGED IN PLACE");
  restoreTerminalViewport(live, anchor);
  expect(
    viewport(live),
    "single-row anchoring cannot protect lower rows",
  ).not.toEqual(before);
  expect(viewport(shown())).toEqual(before);
  view.resume();
  expect(shown()).toBe(live);
  expect(viewport(shown())).toContain("CHANGED IN PLACE");
});

test("reading buffer remains stable through reset, resize and more output", async () => {
  const { live, view, shown } = setup();
  await write(live, "one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven");
  live.scrollLines(-1);
  const before = viewport(live);
  await new Promise<void>((resolve) => view.beforeWrite(resolve));
  const reader = shown();
  live.reset();
  live.resize(40, 4);
  await write(live, "new live screen");
  await new Promise<void>((resolve) => view.beforeWrite(resolve));
  expect(shown()).toBe(reader);
  expect(viewport(shown())).toEqual(before);
  reader.scrollToBottom();
  expect(shown()).toBe(live);
  expect(viewport(live)).toContain("new live screen");
});

test("following the bottom does not allocate a reading buffer", async () => {
  const { live, view, shown } = setup();
  await write(live, "hello");
  await new Promise<void>((resolve) => view.beforeWrite(resolve));
  expect(shown()).toBe(live);
});

test("alternate-screen applications keep their own scrolling", async () => {
  const { live, view, shown } = setup();
  await write(live, "\x1b[?1049hfull-screen app");
  await new Promise<void>((resolve) => view.beforeWrite(resolve));
  expect(shown()).toBe(live);
});

test("teardown cancels an unfinished reading snapshot without resuming writes", async () => {
  const { live, view } = setup();
  await write(live, "one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven");
  live.scrollLines(-1);
  let resumed = false;
  view.beforeWrite(() => {
    resumed = true;
  });
  view.dispose();
  await new Promise((resolve) => setTimeout(resolve, 10));
  expect(resumed).toBe(false);
});
