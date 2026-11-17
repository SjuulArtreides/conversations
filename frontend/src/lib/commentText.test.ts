import { describe, expect, it } from "vitest";
import { cleanCommentForConversation } from "./commentText";

describe("cleanCommentForConversation", () => {
  it("strips HTML comments including the base64 internal-state block", () => {
    const body =
      '<!-- internal state start --> <!-- ' +
      "AbCdEf0123456789== --> <!-- internal state end -->\nReal content.";
    expect(cleanCommentForConversation(body)).toBe("Real content.");
  });

  it("unwraps <details>/<summary>/<a> to their visible text", () => {
    const body =
      '<details> <summary><a href="https://x/PLA-602">PLA-602 Title</a></summary>' +
      " <p>Body sentence here.</p> </details>";
    expect(cleanCommentForConversation(body)).toBe(
      "PLA-602 Title Body sentence here.",
    );
  });

  it("flattens newlines to spaces so words don't run together once injected", () => {
    expect(cleanCommentForConversation("line one\nline two\n\nline three")).toBe(
      "line one line two line three",
    );
  });

  it("decodes the common HTML entities", () => {
    expect(cleanCommentForConversation("a &amp; b &lt;tag&gt; &quot;q&quot;")).toBe(
      'a & b <tag> "q"',
    );
  });

  it("leaves plain text untouched (aside from trimming)", () => {
    expect(cleanCommentForConversation("  Please rename this function.  ")).toBe(
      "Please rename this function.",
    );
  });
});
