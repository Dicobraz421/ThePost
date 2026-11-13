// @vitest-environment node
import { existsSync, readFileSync } from "node:fs";

import { expect, test } from "vitest";

import { generate } from "../../scripts/gen-api.mjs";

import { covered } from "./coverage";
import spec from "./openapi.json";

const here = (name: string) => new URL(name, import.meta.url);

test("типы schema.d.ts совпадают со схемой openapi.json (npm run gen:api)", async () => {
  expect(readFileSync(here("./schema.d.ts"), "utf8")).toBe(await generate());
});

test("operationId операций схемы уникальны: по ним названы типы `operations` в schema.d.ts", () => {
  const ids = Object.values(spec.paths).flatMap((item) => Object.values(item).map((op: { operationId?: string }) => op.operationId));
  expect(ids.filter((id) => !id)).toEqual([]);
  expect(ids.filter((id, i) => ids.indexOf(id) !== i), "повторы").toEqual([]);
});

test("каждая операция OpenAPI — экран консоли или строка в coverage-pending.txt", () => {
  const methods = ["get", "post", "put", "patch", "delete"];
  const operations = Object.entries(spec.paths).flatMap(([path, item]) =>
    Object.keys(item)
      .filter((m) => methods.includes(m))
      .map((m) => `${m.toUpperCase()} ${path}`),
  );
  const pending = readFileSync(here("../../coverage-pending.txt"), "utf8")
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l && !l.startsWith("#"));
  const known = [...Object.keys(covered), ...pending];
  expect(operations.filter((o) => !known.includes(o)), "операции без экрана и без строки ожидания").toEqual([]);
  expect(known.filter((o) => !operations.includes(o)), "операций нет в схеме").toEqual([]);
  expect(pending.filter((o) => o in covered), "экран есть — строку ожидания убрать").toEqual([]);
  for (const screen of Object.values(covered)) expect(existsSync(here(`../${screen}`)), screen).toBe(true);
});
