import { expect, it } from "vitest";
import { checkBody } from "./privateChannel";

const body = {
  kind: "migration",
  manifest: {
    agent: "codex",
    native_id: "00000000-0000-4000-8000-000000000001",
    title: "A session",
    size: 3,
    sha256: "a".repeat(43),
  },
  b64: "YWJj",
  next: null,
};
it("accepts the native migration reply on the shared signed-reply boundary", () => {
  expect(checkBody(JSON.parse(JSON.stringify(body)))).toEqual(body);
});
it.each([
  { ...body, manifest: { ...body.manifest, size: 65 * 1024 * 1024 } },
  { ...body, manifest: { ...body.manifest, native_id: "../../other" } },
  { ...body, manifest: { ...body.manifest, agent: "zsh" } },
  { ...body, manifest: { ...body.manifest, sha256: "invalid" } },
  { ...body, next: 3 },
  { ...body, b64: "a".repeat(8193) },
])("rejects malformed or unsupported migration content", (value) => {
  expect(() => checkBody(value)).toThrow();
});
