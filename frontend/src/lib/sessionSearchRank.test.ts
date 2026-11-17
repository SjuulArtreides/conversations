import { expect, it } from "vitest";
import { exactTitleFirst } from "./sessionSearchRank";
it("ranks literal trimmed case-insensitive exact titles first and preserves each tier's sort", () => {
  const items = [
    { title: "Fix bug later", id: 1 },
    { title: " FIX BUG ", id: 2 },
    { title: "fix bug", id: 3 },
    { title: "Another fix bug", id: 4 },
  ];
  expect(
    exactTitleFirst(items, " Fix Bug ", (v) => v.title).map((v) => v.id),
  ).toEqual([2, 3, 1, 4]);
  expect(exactTitleFirst(items, " ", (v) => v.title)).toEqual(items);
  expect(
    exactTitleFirst(
      [{ title: "100 percent" }, { title: "100%" }],
      "100%",
      (v) => v.title,
    )[0].title,
  ).toBe("100%");
});
