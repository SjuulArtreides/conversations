import { afterEach, describe, expect, it } from "vitest";
import {
  readPreferredFontSize,
  TERMINAL_FONT_SIZE_KEY,
  writePreferredFontSize,
} from "@/lib/terminalFontSize";

describe("preferred terminal font size", () => {
  afterEach(() => localStorage.removeItem(TERMINAL_FONT_SIZE_KEY));

  it("is unset until the viewer saves one", () => {
    expect(readPreferredFontSize()).toBeNull();
    writePreferredFontSize(15);
    expect(readPreferredFontSize()).toBe(15);
    writePreferredFontSize(null);
    expect(readPreferredFontSize()).toBeNull();
  });

  it("clamps a saved value into the allowed range", () => {
    writePreferredFontSize(2);
    expect(readPreferredFontSize()).toBe(8);
    writePreferredFontSize(99.6);
    expect(readPreferredFontSize()).toBe(32);
  });

  it("ignores a stored value it cannot use", () => {
    for (const raw of ["", "abc", "14.5", "4", "100"]) {
      localStorage.setItem(TERMINAL_FONT_SIZE_KEY, raw);
      expect(readPreferredFontSize()).toBeNull();
    }
  });
});
