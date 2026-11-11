import { expect, test } from "vitest";
import { sum } from "./sum";

test("sum", () => {
  expect(sum(2, 3)).toBe(5);
});
