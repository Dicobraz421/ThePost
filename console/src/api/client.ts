// Обращения к Admin API (SRS 5.2): сессия, организация, ошибки RFC 9457.
import type { HttpError } from "@refinedev/core";

import type { components } from "./schema";

export type Schemas = components["schemas"];

export const API = "/api/v1";
const SESSION_KEY = "thepost.session";
const ORG_KEY = "thepost.org";

/** Вход: значение для `Authorization: Bearer` и срок его действия. */
export type Session = { token: string; expires_at: string };

export function loadSession(): Session | null {
  try {
    const s = JSON.parse(sessionStorage.getItem(SESSION_KEY) ?? "null") as Session | null;
    return s && Date.parse(s.expires_at) > Date.now() ? s : null;
  } catch {
    return null;
  }
}

let me: { token?: string; data: Promise<Schemas["WhoAmI"]> } | undefined;

/** Кто вошёл и его роли: один запрос `whoami` на сессию (меню и кнопки спрашивают роли многократно). */
export function whoami() {
  const token = loadSession()?.token;
  if (!me || me.token !== token) {
    const data = request<Schemas["WhoAmI"]>(`${API}/whoami`).then((r) => r.data);
    data.catch(() => (me = undefined));
    me = { token, data };
  }
  return me.data;
}

export function saveSession(s: Session | null) {
  me = undefined;
  if (s) sessionStorage.setItem(SESSION_KEY, JSON.stringify(s));
  else sessionStorage.removeItem(SESSION_KEY);
}

/** Выбранная организация (UUID); по умолчанию — организация вошедшего администратора. */
export const getOrg = () => localStorage.getItem(ORG_KEY) ?? "";
export const setOrg = (org: string) => localStorage.setItem(ORG_KEY, org);

export type RequestOptions = {
  method?: string;
  body?: unknown;
  query?: Record<string, unknown>;
  /** Заменяет токен сессии (обмен токена `admin init`). */
  bearer?: string;
};

/** Запрос к Admin API; `{org}` в пути — выбранная организация. Ошибка — `HttpError` для уведомлений Refine. */
export async function request<T>(path: string, o: RequestOptions = {}): Promise<{ data: T; next?: string }> {
  const url = new URL(path.replace("{org}", encodeURIComponent(getOrg())), location.origin);
  for (const [k, v] of Object.entries(o.query ?? {})) {
    if (v !== undefined && v !== null && v !== "") url.searchParams.set(k, String(v));
  }
  const bearer = o.bearer ?? loadSession()?.token;
  const resp = await fetch(url.pathname + url.search, {
    method: (o.method ?? "GET").toUpperCase(),
    headers: {
      Accept: "application/json",
      ...(bearer ? { Authorization: `Bearer ${bearer}` } : {}),
      ...(o.body === undefined ? {} : { "Content-Type": "application/json" }),
    },
    body: o.body === undefined ? undefined : JSON.stringify(o.body),
  });
  const text = await resp.text();
  let data: unknown;
  try {
    data = text ? JSON.parse(text) : undefined;
  } catch {
    data = undefined;
  }
  if (!resp.ok) {
    const p = (data ?? {}) as Partial<Schemas["Problem"]>;
    // `problem` — тело ответа целиком: расширения RFC 9457 (ошибки текста скрипта Sieve — `errors`).
    const error: HttpError = { message: p.detail ?? p.title ?? resp.statusText, statusCode: resp.status, problem: data };
    throw error;
  }
  // Продолжение списка: `Link: <…&after=…>; rel="next"` (RFC 8288).
  const link = /<([^>]*)>;\s*rel="next"/.exec(resp.headers.get("Link") ?? "");
  const next = link ? (new URL(link[1], location.origin).searchParams.get("after") ?? undefined) : undefined;
  return { data: data as T, next };
}
