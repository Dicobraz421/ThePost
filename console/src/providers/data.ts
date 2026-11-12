// Провайдер данных Refine поверх Admin API: ресурсы организации — `/api/v1/orgs/{org}/<ресурс>`,
// списки — курсоры `after`/`limit`; ошибки API приходят в уведомления как `HttpError`.
import type { DataProvider } from "@refinedev/core";

import { API, request } from "../api/client";

const base = (resource: string) => `${API}/orgs/{org}/${resource}`;
const item = (resource: string, id: unknown) => `${base(resource)}/${encodeURIComponent(String(id))}`;

export const dataProvider: DataProvider = {
  getApiUrl: () => API,
  getList: async ({ resource, pagination, filters, meta }) => {
    // Курсор: `meta.after` или страница бесконечного списка (`useInfiniteList` кладёт курсор в `currentPage`).
    const page: unknown = pagination?.currentPage;
    const query: Record<string, unknown> = { after: meta?.after ?? (typeof page === "string" ? page : undefined) };
    if (pagination?.mode !== "off") query.limit = pagination?.pageSize;
    for (const f of filters ?? []) if ("field" in f && f.operator === "eq") query[f.field] = f.value;
    const { data, next } = await request<never[]>(base(resource), { query });
    return { data, total: data.length, cursor: { next } };
  },
  getOne: async ({ resource, id }) => ({ data: (await request<never>(item(resource, id))).data }),
  create: async ({ resource, variables }) => ({
    data: (await request<never>(base(resource), { method: "POST", body: variables })).data,
  }),
  update: async ({ resource, id, variables, meta }) => ({
    data: (await request<never>(item(resource, id), { method: meta?.method ?? "PUT", body: variables })).data,
  }),
  deleteOne: async ({ resource, id }) => ({
    data: (await request<never>(item(resource, id), { method: "DELETE" })).data,
  }),
  // Операции вне схемы «ресурс/идентификатор»: `url` — путь схемы OpenAPI (`/api/v1/orgs/{org}/policy`).
  custom: async ({ url, method, payload, query }) => ({
    data: (await request<never>(url, { method, body: payload, query: query as Record<string, unknown> })).data,
  }),
};
