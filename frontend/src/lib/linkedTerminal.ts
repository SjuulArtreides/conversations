import {
  Terminal,
  type ITerminalOptions,
  type ITerminalInitOnlyOptions,
} from "@xterm/xterm";
import { WebLinksAddon } from "@xterm/addon-web-links";

/** Shared construction for the live terminal and its frozen reading view. */
export function createLinkedTerminal(
  options: ITerminalOptions & ITerminalInitOnlyOptions,
) {
  const hover = { active: false };
  const enter = () => void (hover.active = true);
  const leave = () => void (hover.active = false);
  const terminal = new Terminal({
    ...options,
    linkHandler: {
      activate: (_event, uri) => openLink(uri),
      hover: enter,
      leave,
    },
  });
  hoveredLinks.set(terminal, hover);
  // Also detect plain URLs emitted without OSC 8 metadata.
  terminal.loadAddon(new WebLinksAddon(undefined, { hover: enter, leave }));
  return terminal;
}

const hoveredLinks = new WeakMap<Terminal, { active: boolean }>();

// A copy of xterm 6.0.0's default OSC 8 `activate`. We must supply a
// `linkHandler` to observe hover, which replaces the built-in, and
// `ILinkHandler` requires `activate`. Keep the confirm: an OSC 8 label can
// differ from its target. xterm only registers http(s) OSC 8 links (we do not
// set `allowNonHttpProtocols`), so no protocol check is needed here.
function openLink(uri: string) {
  if (
    !confirm(
      `Do you want to navigate to ${uri}?\n\nWARNING: This link could potentially be dangerous`,
    )
  ) {
    return;
  }
  const popup = window.open();
  if (!popup) {
    console.warn("Opening link blocked as opener could not be cleared");
    return;
  }
  try {
    popup.opener = null;
  } catch {
    // Match xterm: a failure to clear the opener is ignored.
  }
  popup.location.href = uri;
}

/* eslint-disable no-control-regex -- terminal escape sequences */
const SGR_MOUSE = /^\x1b\[<(\d+);\d+;\d+([Mm])$/;
const X10_MOUSE = /^\x1b\[M([\s\S])/;
/* eslint-enable no-control-regex */
const MOTION_OR_WHEEL = 32 | 64;

/**
 * xterm activates a link on a click and, with mouse tracking on, still reports
 * that click to the program. A TUI that opens URLs itself then opens a second
 * tab on the host. The returned filter says whether input should be forwarded,
 * dropping the press and release of a click that started over a link. Wheel and
 * motion reports, and clicks elsewhere, pass through.
 */
export function createLinkClickFilter(terminal: Terminal) {
  let swallowRelease = false;
  const overLink = () => hoveredLinks.get(terminal)?.active === true;

  return (data: string): boolean => {
    let code: number;
    let release: boolean;
    const sgr = SGR_MOUSE.exec(data);
    const x10 = !sgr && data.length === 6 ? X10_MOUSE.exec(data) : null;
    if (sgr) {
      code = Number(sgr[1]);
      release = sgr[2] === "m";
    } else if (x10) {
      code = x10[1].charCodeAt(0) - 32;
      release = (code & 3) === 3;
    } else {
      return true;
    }
    if (code & MOTION_OR_WHEEL) return true;
    if (!release) {
      swallowRelease = overLink();
      return !swallowRelease;
    }
    const swallow = swallowRelease;
    swallowRelease = false;
    return !swallow;
  };
}
