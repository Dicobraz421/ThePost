// Типы Admin API из схемы узла: `npm run gen:api` пишет src/api/schema.d.ts, тест сверяет файл со схемой.
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import openapiTS, { astToString, COMMENT_HEADER } from "openapi-typescript";

const file = (name) => fileURLToPath(new URL(`../src/api/${name}`, import.meta.url));

export async function generate() {
  const spec = JSON.parse(readFileSync(file("openapi.json"), "utf8"));
  // operationId в схеме узла повторяются (list_domains — у доменов и у transport/domains и др.), а типы операций
  // openapi-typescript именует по operationId: без него типы операций встраиваются в `paths` и не сталкиваются.
  for (const item of Object.values(spec.paths)) for (const op of Object.values(item)) delete op.operationId;
  return COMMENT_HEADER + astToString(await openapiTS(spec));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) writeFileSync(file("schema.d.ts"), await generate());
