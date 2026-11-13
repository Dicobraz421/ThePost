// Типы Admin API из схемы узла: `npm run gen:api` пишет src/api/schema.d.ts, тест сверяет файл со схемой.
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import openapiTS, { astToString, COMMENT_HEADER } from "openapi-typescript";

const file = (name) => fileURLToPath(new URL(`../src/api/${name}`, import.meta.url));

export async function generate() {
  const spec = JSON.parse(readFileSync(file("openapi.json"), "utf8"));
  return COMMENT_HEADER + astToString(await openapiTS(spec));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) writeFileSync(file("schema.d.ts"), await generate());
