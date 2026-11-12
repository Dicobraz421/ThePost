import { beforeEach, expect, test } from "vitest";

import { loadSession } from "../api/client";
import { ORG, session, stubApi, whoami } from "../test-api";
import { authProvider, sessionInfo } from "./auth";
import { dataProvider } from "./data";

beforeEach(() => {
  sessionStorage.clear();
  localStorage.clear();
});

const signIn = async () => {
  stubApi({ "POST /api/v1/session": { status: 201, body: session() } });
  await authProvider.login({ token: "tpa_once" });
};

test("вход по токену: обмен на сессию, организация администратора, выход", async () => {
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

test("поставщик OIDC — из GET /api/v1/session; операции нет — только токен", async () => {
  const oidc = { issuer: "https://idp.example.org", client_id: "thepost-console" };
  stubApi({ "GET /api/v1/session": { body: { oidc } } });
  expect(await sessionInfo()).toEqual({ oidc });
  stubApi({ "GET /api/v1/session": { status: 405, body: { status: 405, title: "Method Not Allowed" } } });
  expect(await sessionInfo()).toEqual({});
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
