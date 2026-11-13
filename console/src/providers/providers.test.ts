import { afterEach, beforeEach, expect, test } from "vitest";

import { loadConfig, loadSession } from "../api/client";
import { ORG, session, stubApi, whoami } from "../test-api";
import { authProvider, sessionInfo } from "./auth";
import { dataProvider } from "./data";

beforeEach(() => {
  sessionStorage.clear();
  localStorage.clear();
});

// Адрес API — состояние модуля: после теста с config.json — снова источник страницы.
afterEach(async () => {
  stubApi({});
  await loadConfig();
});

const signIn = async () => {
  stubApi({ "POST /api/v1/session": { status: 201, body: session() } });
  await authProvider.login({ login: "admin@example.org", password: "s3cret" });
};

test("вход по паролю: тело PasswordLogin без токена, сессия, организация администратора", async () => {
  const calls = stubApi({ "POST /api/v1/session": { status: 201, body: session() } });
  expect(await authProvider.login({ login: " admin@example.org ", password: "s3cret" })).toMatchObject({
    success: true,
    redirectTo: "/",
  });
  expect(calls[0]).toMatchObject({ method: "POST", path: "/api/v1/session" });
  expect(calls[0].auth).toBeFalsy();
  expect(calls[0].body).toEqual({ login: "admin@example.org", password: "s3cret" });
  expect(loadSession()?.token).toBe("tps_test");
  expect(localStorage.getItem("thepost.org")).toBe(ORG);

  stubApi({ "POST /api/v1/session": { status: 401, body: { status: 401, title: "Unauthorized" } } });
  expect(await authProvider.login({ login: "admin@example.org", password: "wrong" })).toMatchObject({
    success: false,
    error: { message: "Unauthorized" },
  });
});

test("вход по токену (вход по паролю выключен): обмен на сессию, выход", async () => {
  const calls = stubApi({ "POST /api/v1/session": { status: 201, body: session() } });
  expect(await authProvider.login({ token: " tpa_once " })).toMatchObject({ success: true, redirectTo: "/" });
  expect(calls[0]).toMatchObject({ method: "POST", path: "/api/v1/session", auth: "Bearer tpa_once" });
  expect(loadSession()?.token).toBe("tps_test");
  expect(await authProvider.check()).toEqual({ authenticated: true });

  const api = stubApi({ "GET /api/v1/whoami": { body: whoami } });
  expect(await authProvider.getIdentity?.()).toEqual(whoami);
  expect(await authProvider.getPermissions?.()).toEqual(["installation_admin"]);
  expect(api[0].auth).toBe("Bearer tps_test");

  await authProvider.logout({});
  expect(await authProvider.check()).toMatchObject({ authenticated: false, redirectTo: "/login" });
});

test("отказ во входе — ошибка с текстом API; истёкшая сессия и 401 — на экран входа", async () => {
  stubApi({ "POST /api/v1/session": { status: 401, body: { status: 401, title: "Unauthorized", detail: "токен погашен" } } });
  expect(await authProvider.login({ token: "tpa_used" })).toMatchObject({
    success: false,
    error: { message: "токен погашен" },
  });
  expect(await authProvider.check()).toMatchObject({ authenticated: false });

  stubApi({ "POST /api/v1/session": { status: 201, body: session(-1000) } });
  await authProvider.login({ token: "tpa_once" });
  expect(await authProvider.check()).toMatchObject({ authenticated: false, logout: true });

  expect(await authProvider.onError({ statusCode: 401 })).toMatchObject({ logout: true });
  expect(await authProvider.onError({ statusCode: 409 })).toEqual({});
});

test("способы входа — из GET /api/v1/session (SessionInfo); узел недоступен — вход по паролю", async () => {
  const oidc = { issuer: "https://idp.example.org", client_id: "thepost-console", scope: "openid profile" };
  const calls = stubApi({ "GET /api/v1/session": { body: { oidc, password: false } } });
  expect(await sessionInfo()).toEqual({ oidc, password: false });
  expect(calls[0]).toMatchObject({ method: "GET", path: "/api/v1/session" });
  stubApi({ "GET /api/v1/session": { body: { oidc: null, password: true } } });
  expect(await sessionInfo()).toEqual({ oidc: null, password: true });
  stubApi({ "GET /api/v1/session": { status: 503, body: { status: 503, title: "Service Unavailable" } } });
  expect(await sessionInfo()).toEqual({ oidc: null, password: true });
});

const json = { "Content-Type": "application/json" };
// `config.json` — рядом с `index.html`: каталог сборки (`base` Vite; в тестах — корень).
const CONFIG = `${import.meta.env.BASE_URL}config.json`;

test("config.json: запросы входа и провайдера данных — на apiBase, продолжение списка — оттуда же", async () => {
  const API = "https://mail.example.org:8443";
  const config = stubApi({ [`GET ${CONFIG}`]: { body: { apiBase: `${API}/` }, headers: json } });
  await loadConfig();
  expect(config[0].path).toBe(CONFIG);

  const calls = stubApi({
    [`POST ${API}/api/v1/session`]: { status: 201, body: session() },
    [`GET ${API}/api/v1/orgs/${ORG}/accounts`]: {
      body: [{ id: "1" }],
      headers: { Link: `<${API}/api/v1/orgs/${ORG}/accounts?after=x@example.org>; rel="next"` },
    },
  });
  expect(await authProvider.login({ token: "tpa_once" })).toMatchObject({ success: true });
  const page = await dataProvider.getList({ resource: "accounts", pagination: { mode: "off" } });
  expect(calls.map((c) => c.path)).toEqual([`${API}/api/v1/session`, `${API}/api/v1/orgs/${ORG}/accounts`]);
  expect(page.cursor).toEqual({ next: "x@example.org" });
});

test("config.json: нет файла, страница вместо него или нет поля — источник страницы; не источник — ошибка", async () => {
  const replies = [
    { status: 404 },
    { body: "<!doctype html>", headers: { "Content-Type": "text/html" } },
    { body: {}, headers: json },
  ];
  for (const reply of replies) {
    stubApi({ [`GET ${CONFIG}`]: reply });
    await loadConfig();
    const calls = stubApi({ "GET /api/v1/session": { body: { oidc: null, password: true } } });
    await sessionInfo();
    expect(calls[0].path).toBe("/api/v1/session");
  }
  for (const apiBase of ["https://mail.example.org/api", "mail.example.org", 8443]) {
    stubApi({ [`GET ${CONFIG}`]: { body: { apiBase }, headers: json } });
    await expect(loadConfig()).rejects.toThrow("config.json: apiBase");
  }
});

test("список: организация в пути, курсор after/limit, продолжение из Link", async () => {
  await signIn();
  const calls = stubApi({
    [`GET /api/v1/orgs/${ORG}/accounts`]: {
      body: [{ id: "1" }, { id: "2" }],
      headers: { Link: `</api/v1/orgs/${ORG}/accounts?limit=2&after=b%2Bx@example.org>; rel="next"` },
    },
  });
  const page = await dataProvider.getList({
    resource: "accounts",
    pagination: { pageSize: 2 },
    filters: [{ field: "prefix", operator: "eq", value: "b" }],
    meta: { after: "a@example.org" },
  });
  expect(calls[0].path).toBe(`/api/v1/orgs/${ORG}/accounts?after=a%40example.org&limit=2&prefix=b`);
  expect(calls[0].auth).toBe("Bearer tps_test");
  expect(page).toEqual({ data: [{ id: "1" }, { id: "2" }], total: 2, cursor: { next: "b+x@example.org" } });
});

test("чтение, создание, замена, удаление и произвольная операция", async () => {
  await signIn();
  const base = `/api/v1/orgs/${ORG}/groups`;
  const calls = stubApi({
    [`GET ${base}/d1`]: { body: { id: "d1", name: "example.org" } },
    [`POST ${base}`]: { status: 201, body: { id: "d2", name: "example.net" } },
    [`PUT ${base}/d1`]: { body: { id: "d1" } },
    [`DELETE ${base}/d1`]: { status: 204 },
    [`PUT /api/v1/orgs/${ORG}/policy`]: (c) => ({ body: c.body }),
  });
  expect((await dataProvider.getOne({ resource: "groups", id: "d1" })).data).toEqual({ id: "d1", name: "example.org" });
  expect((await dataProvider.create({ resource: "groups", variables: { name: "example.net" } })).data).toMatchObject({ id: "d2" });
  await dataProvider.update({ resource: "groups", id: "d1", variables: {} });
  await dataProvider.deleteOne({ resource: "groups", id: "d1" });
  const custom = await dataProvider.custom?.({ url: "/api/v1/orgs/{org}/policy", method: "put", payload: { quota_bytes: 5 } });
  expect(custom?.data).toEqual({ quota_bytes: 5 });
  expect(calls.map((c) => c.method)).toEqual(["GET", "POST", "PUT", "DELETE", "PUT"]);
  expect(calls[1].body).toEqual({ name: "example.net" });
});

test("ошибка API (RFC 9457) — HttpError для уведомления", async () => {
  await signIn();
  stubApi({ [`POST /api/v1/orgs/${ORG}/groups`]: { status: 409, body: { status: 409, title: "Conflict", detail: "группа уже есть" } } });
  await expect(dataProvider.create({ resource: "groups", variables: { name: "all@example.org" } })).rejects.toMatchObject({
    message: "группа уже есть",
    statusCode: 409,
  });
});
