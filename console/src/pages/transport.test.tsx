import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, expect, test } from "vitest";

import { BASE, buttons, open, sent } from "../test-app";
import { ORG } from "../test-api";

const T = `${BASE}/transport`;
const domain = { id: "d1", tenant: ORG, name: "example.org", kind: "authoritative" };
const relay = { id: "d2", tenant: ORG, name: "old.example.org", kind: { internal_relay: { next_hop: "old-mail:25" } } };
const gateway = { name: "mx", tenant: ORG, matcher: { networks: ["192.0.2.0/24"] }, kind: { gateway: { xclient: true, authserv_id: "mx.example.org" } } };
const shared = { name: "edge", tenant: null, matcher: { client_cert: { subject: "edge.example.net" } }, kind: { gateway: { xclient: false, authserv_id: null } } };
const route = { pattern: "partner.example", tls: { pinned: { spki_sha256: ["ab"] } }, smart_host: null, egress_ip: null };
const routes = {
  [`GET ${T}/domains`]: { body: { version: 7, domains: [domain, relay] } },
  [`GET ${T}/sources`]: { body: { version: 7, sources: [gateway] } },
  [`GET ${T}/routes`]: { body: { version: 3, routes: [route] } },
  "GET /api/v1/transport": { body: { version: 7, domains: [domain, relay], sources: [gateway, shared] } },
};
/** Последняя кнопка с этим именем — кнопка окна или подтверждения. */
const last = async (name: string) => (await buttons(name)).at(-1) as HTMLButtonElement;

afterEach(() => {
  cleanup();
  sessionStorage.clear();
  localStorage.clear();
});

test("принятые домены: список, добавление с версией снимка, удаление с подтверждением", async () => {
  const calls = open("/transport", {
    ...routes,
    [`POST ${T}/domains`]: { status: 201, body: { version: 8, domain } },
    [`DELETE ${T}/domains/d2`]: { body: { version: 8 } },
  });
  expect(await screen.findByText("old.example.org")).toBeTruthy();
  expect(screen.getByText("внутренняя ретрансляция → old-mail:25")).toBeTruthy();

  fireEvent.click(screen.getByRole("button", { name: "Добавить принятый домен" }));
  fireEvent.change(await screen.findByLabelText("Имя домена"), { target: { value: " new.example.org " } });
  fireEvent.click(await last("Сохранить"));
  await waitFor(() =>
    expect(sent(calls, "POST", `${T}/domains`)[0]?.body).toEqual({ version: 7, name: "new.example.org", kind: "authoritative" }),
  );
  // После записи списки перечитываются: версия снимка кластера у доменов и источников общая.
  await waitFor(() => expect(sent(calls, "GET", `${T}/sources`).length).toBeGreaterThan(1));

  fireEvent.click(screen.getAllByRole("button", { name: "Удалить" })[1]);
  expect(await screen.findByText("Удалить принятый домен old.example.org?")).toBeTruthy();
  fireEvent.click(await last("Удалить"));
  await waitFor(() => expect(sent(calls, "DELETE", `${T}/domains/d2?version=7`)).toHaveLength(1));
});

test("политика домена: окно у строки принятого домена — GET и PUT transport/domains/{id}/policy", async () => {
  const empty = {
    dkim_selector: null,
    max_recipients: null,
    milter_profile: null,
    quota_bytes: null,
    rcpts_per_hour: null,
    retention: null,
    send_per_hour: null,
    trash_purge: null,
    trash_retention: null,
  };
  const calls = open("/transport", {
    ...routes,
    [`GET ${T}/domains/d1/policy`]: { body: { policy: { ...empty, dkim_selector: "s1" }, effective: { ...empty, dkim_selector: "s1" } } },
    [`PUT ${T}/domains/d1/policy`]: (c) => ({ body: c.body }),
  });
  expect(await screen.findByText("old.example.org")).toBeTruthy();
  fireEvent.click(screen.getAllByRole("button", { name: "Политика" })[0]);
  expect(await screen.findByText("Политика домена example.org")).toBeTruthy();
  const selector = () => screen.getByLabelText("Селектор ключа DKIM.") as HTMLInputElement;
  await waitFor(() => expect(selector().value).toBe("s1"));
  fireEvent.change(selector(), { target: { value: "s2" } });
  fireEvent.click(screen.getByRole("button", { name: "Сохранить политику" }));
  await waitFor(() => expect(sent(calls, "PUT", `${T}/domains/d1/policy`)[0]?.body).toEqual({ ...empty, dkim_selector: "s2" }));
  expect(await screen.findByText("Политика домена сохранена")).toBeTruthy();
});

test("источники и маршруты: изменение источника — PUT по имени, новый маршрут — PUT по шаблону", async () => {
  const calls = open("/transport", {
    ...routes,
    [`PUT ${T}/sources/mx`]: { body: { version: 8 } },
    [`PUT ${T}/routes/*.example.net`]: { body: { version: 4 } },
  });
  fireEvent.click(await screen.findByRole("tab", { name: "Доверенные источники" }));
  expect(await screen.findByText("шлюз, XCLIENT, authserv-id mx.example.org")).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "Изменить" }));
  fireEvent.change(await screen.findByLabelText("Доверять проверкам шлюза (authserv-id)"), { target: { value: "" } });
  fireEvent.click(await last("Сохранить"));
  await waitFor(() =>
    expect(sent(calls, "PUT", `${T}/sources/mx`)[0]?.body).toEqual({
      version: 7,
      matcher: { networks: ["192.0.2.0/24"] },
      kind: { gateway: { xclient: true, authserv_id: null } },
    }),
  );

  fireEvent.click(screen.getByRole("tab", { name: "Маршруты" }));
  expect(await screen.findByText("закреплённый ключ (1)")).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "Добавить маршрут" }));
  fireEvent.change(await screen.findByLabelText("Домен назначения"), { target: { value: "*.example.net" } });
  fireEvent.change(screen.getByLabelText("Промежуточный узел"), { target: { value: "relay.example.net:587" } });
  fireEvent.click(await last("Сохранить"));
  await waitFor(() =>
    expect(sent(calls, "PUT", `${T}/routes/*.example.net`)[0]?.body).toEqual({
      version: 3,
      tls: "opportunistic",
      smart_host: "relay.example.net:587",
      egress_ip: null,
    }),
  );
});

test("инсталляция: источники кластера — /api/v1/transport; источник организации там не правится", async () => {
  const calls = open("/transport", { ...routes, "DELETE /api/v1/transport/sources/edge": { body: { version: 8 } } });
  fireEvent.click(await screen.findByRole("tab", { name: "Инсталляция" }));
  expect(await screen.findByText("сертификат: edge.example.net")).toBeTruthy();
  expect(screen.getByText("кластер")).toBeTruthy();
  // Организация — по имени из GET /api/v1/orgs: владелец источника mx и оба принятых домена.
  expect(within(screen.getByRole("tabpanel")).getAllByText("по умолчанию")).toHaveLength(3);
  // Кнопки — только у источника кластера.
  expect(screen.getAllByRole("button", { name: "Удалить" })).toHaveLength(1);
  fireEvent.click(screen.getByRole("button", { name: "Удалить" }));
  await screen.findByText("Удалить доверенный источник edge?");
  fireEvent.click(await last("Удалить"));
  await waitFor(() => expect(sent(calls, "DELETE", "/api/v1/transport/sources/edge?version=7")).toHaveLength(1));
});

test("оператор: транспорт только для чтения, раздела инсталляции нет", async () => {
  const calls = open("/transport", routes, ["operator"]);
  expect(await screen.findByText("old.example.org")).toBeTruthy();
  await waitFor(() => expect(screen.queryByRole("button", { name: "Добавить принятый домен" })).toBeNull());
  expect(screen.queryByRole("button", { name: "Удалить" })).toBeNull();
  expect(screen.queryByRole("tab", { name: "Инсталляция" })).toBeNull();
  expect(sent(calls, "GET", "/api/v1/transport")).toHaveLength(0);
});
