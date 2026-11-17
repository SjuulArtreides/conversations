import { describe, expect, it } from "vitest";
import {
  codeRabbitPreview,
  codeRabbitSeverity,
  isCodeRabbitAuthor,
  isCodeRabbitRollup,
} from "./coderabbit";

describe("isCodeRabbitAuthor", () => {
  it("matches the bot login with or without the [bot] suffix, case-insensitively", () => {
    expect(isCodeRabbitAuthor("coderabbitai")).toBe(true);
    expect(isCodeRabbitAuthor("coderabbitai[bot]")).toBe(true);
    expect(isCodeRabbitAuthor("CodeRabbitAI")).toBe(true);
    expect(isCodeRabbitAuthor("  coderabbitai  ")).toBe(true);
  });

  it("does not match humans or look-alikes", () => {
    expect(isCodeRabbitAuthor("mharkonnen")).toBe(false);
    expect(isCodeRabbitAuthor("coderabbit-fan")).toBe(false);
    expect(isCodeRabbitAuthor("")).toBe(false);
    expect(isCodeRabbitAuthor(null)).toBe(false);
    expect(isCodeRabbitAuthor(undefined)).toBe(false);
  });
});

describe("isCodeRabbitRollup", () => {
  it("flags CodeRabbit review summaries and issue/status posts", () => {
    expect(isCodeRabbitRollup({ author: "coderabbitai", kind: "review" })).toBe(
      true,
    );
    expect(isCodeRabbitRollup({ author: "coderabbitai", kind: "issue" })).toBe(
      true,
    );
  });

  it("keeps CodeRabbit inline findings (they carry a resolution state)", () => {
    expect(
      isCodeRabbitRollup({ author: "coderabbitai", kind: "review_comment" }),
    ).toBe(false);
  });

  it("never flags a human's review summary or issue comment", () => {
    expect(isCodeRabbitRollup({ author: "mharkonnen", kind: "review" })).toBe(
      false,
    );
    expect(isCodeRabbitRollup({ author: "mharkonnen", kind: "issue" })).toBe(
      false,
    );
  });
});

describe("codeRabbitSeverity", () => {
  it("parses each severity from the leading metadata badge", () => {
    const badge = (sev: string) =>
      `_📐 Maintainability & Code Quality_ | _${sev}_ | _⚡ Quick win_\n\n**Body.**`;
    expect(codeRabbitSeverity(badge("🔴 Critical"))).toBe("critical");
    expect(codeRabbitSeverity(badge("🟠 Major"))).toBe("major");
    expect(codeRabbitSeverity(badge("🟡 Minor"))).toBe("minor");
    expect(codeRabbitSeverity(badge("🔵 Trivial"))).toBe("trivial");
  });

  it("returns null when there is no severity badge", () => {
    expect(codeRabbitSeverity("**Actionable comments posted: 4**")).toBeNull();
    expect(codeRabbitSeverity("just a normal comment")).toBeNull();
    expect(codeRabbitSeverity("")).toBeNull();
  });

  it("only reads the first line, so a body that mentions a severity word later does not false-positive", () => {
    const body = "**Nothing here.**\n\nThis change is not critical 🔴 at all.";
    expect(codeRabbitSeverity(body)).toBeNull();
  });
});

describe("codeRabbitPreview", () => {
  it("extracts the first bold heading and collapses whitespace", () => {
    const body =
      "_📐 Maintainability & Code Quality_ | _🟡 Minor_ | _⚡ Quick win_\n\n" +
      "**Test-index line   numbers are stale.**\n\nMore detail follows.";
    expect(codeRabbitPreview(body)).toBe("Test-index line numbers are stale.");
  });

  it("returns null when there is no bold heading", () => {
    expect(codeRabbitPreview("no heading here")).toBeNull();
    expect(codeRabbitPreview("**  **")).toBeNull();
  });
});
