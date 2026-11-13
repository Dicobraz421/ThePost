// Заглушка Admin API для тестов: подменяет fetch, запоминает запросы.
import { vi } from "vitest";

export type Call = { method: string; path: string; auth: string | null; body: unknown };
type Reply = { status?: number; body?: unknown; headers?: Record<string, string> };

/** `routes`: «МЕТОД путь» (без строки запроса) → ответ; неизвестный маршрут — 404 problem+json. */
export function stubApi(routes: Record<string, Reply | ((c: Call) => Reply)>) {
  const calls: Call[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: string, init: RequestInit = {}) => {
      const headers = new Headers(init.headers);
      const call: Call = {
        method: init.method ?? "GET",
        path: input,
        auth: headers.get("Authorization"),
        body: init.body ? JSON.parse(String(init.body)) : undefined,
      };
      calls.push(call);
      const route = routes[`${call.method} ${input.split("?")[0]}`];
      const r = (typeof route === "function" ? route(call) : route) ?? {
        status: 404,
        body: { status: 404, title: "Not Found" },
      };
      const status = r.status ?? 200;
      return new Response(r.body === undefined || status === 204 ? null : JSON.stringify(r.body), {
        status,
        headers: r.headers,
      });
    }),
  );
  return calls;
}

export const ORG = "00000000-0000-0000-0000-000000000001";
const HOUR = 3_600_000;
export const session = (expiresIn = HOUR) => ({
  session: "tps_test",
  expires_at: new Date(Date.now() + expiresIn).toISOString(),
  tenant: ORG,
  account: "a1",
  login: "admin@example.org",
  roles: ["installation_admin"],
});
/** Организации `GET /api/v1/orgs`: по умолчанию и вторая (модуль «Мультитенантность»). */
export const OTHER = "00000000-0000-0000-0000-000000000002";
export const orgs = [
  { id: ORG, default: true },
  { id: OTHER, default: false },
];
export const whoami = { tenant: ORG, account: "a1", login: "admin@example.org", roles: ["installation_admin"] };
