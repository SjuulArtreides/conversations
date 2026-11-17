import { render, screen, cleanup } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { PrReviewIcon } from "./PrReviewIcon";
import type { PrStatus } from "@/lib/types";
afterEach(cleanup);
const status: PrStatus = {
  state: "OPEN",
  merged: false,
  draft: false,
  title: "Test",
};
it.each([
  ["APPROVED", "Approved", "text-green-400"],
  ["CHANGES_REQUESTED", "Changes requested", "text-red-400"],
])("renders %s", (decision, label, color) => {
  render(<PrReviewIcon status={{ ...status, review_decision: decision }} />);
  expect(screen.getByRole("img", { name: label }).className).toContain(color);
});
it.each([
  null,
  status,
  { ...status, review_decision: "REVIEW_REQUIRED" },
  { ...status, review_decision: "" },
])("omits undecided reviews", (value) => {
  render(<PrReviewIcon status={value} />);
  expect(screen.queryByRole("img")).toBeNull();
});
