import { cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, test } from "vitest";

import type { Schemas } from "../api/client";
import { BASE, open, sent } from "../test-app";

import { toBody, toForm } from "./groups";

const group = (id: string, name: string, more: object = {}) =>
  ({
    id,
    name,
    address: `${id}@example.org`,
    external_id: null,
    hidden_from_gal: false,
    members: [],
    moderators: [],
    senders: "anyone",
    names: {},
    ...more,
  }) as Schemas["GroupInfo"];
const held = (id: string, more: object = {}) => ({
  id,
  source: "group",
  group: "g1",
  rule: null,
  sender: `${id}@ext.example`,
  sender_account: null,
  recipients: ["g1@example.org"],
  moderators: [],
  message_id: null,
  size: 1024,
  held_at: "2026-11-12T08:00:00Z",
  ...more,
});

afterEach(() => {
  cleanup();
  sessionStorage.clear();
  localStorage.clear();
});

test("группа ↔ форма: участники, отправители; у группы каталога имя, адрес и состав не отправляются", () => {
  const g = group("g1", "Staff", {
    members: [{ account: "a1" }, { group: "g2" }],
    senders: { only: { accounts: ["a2"], groups: ["g3"] } },
    moderators: ["a1"],
  });
  const form = toForm(g);
  expect(form).toMatchObject({ member_accounts: ["a1"], member_groups: ["g2"], senders: "only", only_accounts: ["a2"], only_groups: ["g3"] });
  expect(toBody(form)).toEqual({
    name: "Staff",
    address: "g1@example.org",
    members: [{ account: "a1" }, { group: "g2" }],
    hidden_from_gal: false,
    senders: { only: { accounts: ["a2"], groups: ["g3"] } },
    moderators: ["a1"],
  });
  expect(toBody({ ...form, senders: "internal" }, true)).toEqual({ hidden_from_gal: false, senders: "internal", moderators: ["a1"] });
  expect(toBody({ ...toForm(), name: "X", address: " " })).toMatchObject({ address: null, members: [], senders: "anyone" });
});

test("группы: список, признаки, удаление; группу каталога удалить нельзя", async () => {
  const calls = open("/groups", {
    [`GET ${BASE}/groups`]: {
      body: [group("g1", "Staff", { moderators: ["a1"], members: [{ account: "a1" }] }), group("g2", "Отдел", { external_id: "cn=dept" })],
    },
    [`DELETE ${BASE}/groups/g1`]: { status: 204 },
  });
  expect(await screen.findByText("Staff")).toBeTruthy();
  expect(screen.getByText("модерация")).toBeTruthy();
  expect(screen.getByText("каталог")).toBeTruthy();
  const remove = await waitFor(() => {
    const found = screen.getAllByRole("button", { name: /Удалить/ }) as HTMLButtonElement[];
    expect(found.map((b) => b.disabled)).toEqual([false, true]);
    return found;
  });
  fireEvent.click(remove[0]);
  expect(await screen.findByText(/Удалить группу Staff\?/)).toBeTruthy();
  const buttons = screen.getAllByRole("button", { name: /Удалить/ });
  fireEvent.click(buttons[buttons.length - 1]);
  await waitFor(() => expect(sent(calls, "DELETE", `${BASE}/groups/g1`)).toHaveLength(1));
});

test("группа: создание — POST groups", async () => {
  const calls = open("/groups/create", {
    [`GET ${BASE}/groups`]: { body: [] },
    [`GET ${BASE}/accounts`]: { body: [] },
    [`POST ${BASE}/groups`]: (c) => ({ status: 201, body: group("g9", "Новая", c.body as object) }),
  });
  fireEvent.change(await screen.findByLabelText("Имя группы"), { target: { value: "Новая" } });
  fireEvent.change(screen.getByLabelText("Адрес рассылки"), { target: { value: "new@example.org" } });
  fireEvent.click(screen.getByRole("button", { name: /Сохранить/ }));
  await waitFor(() =>
    expect(sent(calls, "POST", `${BASE}/groups`)[0]?.body).toEqual({
      name: "Новая",
      address: "new@example.org",
      members: [],
      hidden_from_gal: false,
      senders: "anyone",
      moderators: [],
    }),
  );
});

test("группа каталога: изменение — PATCH без имени, адреса и состава; очередь модерации группы", async () => {
  const g = group("g1", "Отдел", {
    external_id: "cn=dept",
    members: [{ account: "a1" }, { account: "a9" }],
    moderators: ["a1"],
    names: { a1: "alice@example.org" },
  });
  const calls = open("/groups/g1", {
    [`GET ${BASE}/groups/g1`]: { body: g },
    [`GET ${BASE}/groups`]: { body: [g] },
    [`GET ${BASE}/accounts`]: { body: [] },
    [`GET ${BASE}/moderation`]: { body: [held("h1")] },
    [`PATCH ${BASE}/groups/g1`]: { body: g },
  });
  const name = (await screen.findByLabelText("Имя группы")) as HTMLInputElement;
  expect(name.value).toBe("Отдел");
  expect(name.disabled).toBe(true);
  // Подписи участников и модераторов — имена входа из ответа (`names`), без запросов по каждой записи.
  expect((await screen.findAllByText("alice@example.org")).length).toBe(2);
  expect(screen.getByText("a9 (записи нет)")).toBeTruthy();
  expect(calls.filter((c) => c.path.startsWith(`${BASE}/accounts/`))).toEqual([]);
  // Очередь — только этой группы.
  expect(await screen.findByText("h1@ext.example")).toBeTruthy();
  expect(sent(calls, "GET", `${BASE}/moderation?group=g1`)).toHaveLength(1);

  fireEvent.click(screen.getByLabelText("Скрыть из адресной книги"));
  fireEvent.click(screen.getByRole("button", { name: /Сохранить/ }));
  await waitFor(() =>
    expect(sent(calls, "PATCH", `${BASE}/groups/g1`)[0]?.body).toEqual({ hidden_from_gal: true, senders: "anyone", moderators: ["a1"] }),
  );
  expect(await screen.findByText("Группа сохранена")).toBeTruthy();
});

test("очередь модерации: одобрить и отклонить с причиной", async () => {
  let queue = [held("h1"), held("h2", { source: "flow_rule", group: null, rule: "dlp" })];
  const done = (id: string) => () => {
    queue = queue.filter((h) => h.id !== id);
    return { status: 204 };
  };
  const calls = open("/moderation", {
    [`GET ${BASE}/moderation`]: () => ({ body: queue }),
    [`GET ${BASE}/groups`]: { body: [group("g1", "Staff")] },
    [`POST ${BASE}/moderation/h1/approve`]: done("h1"),
    [`POST ${BASE}/moderation/h2/reject`]: done("h2"),
  });
  expect(await screen.findByText("h1@ext.example")).toBeTruthy();
  expect(await screen.findByText("группа «Staff»")).toBeTruthy();
  expect(screen.getByText("правило потока «dlp»")).toBeTruthy();

  fireEvent.click(screen.getAllByRole("button", { name: "Одобрить" })[0]);
  await waitFor(() => expect(screen.queryByText("h1@ext.example")).toBeNull());
  expect(sent(calls, "POST", `${BASE}/moderation/h1/approve`)[0]?.body).toBeUndefined();
  expect(await screen.findByText("Письмо одобрено")).toBeTruthy();

  fireEvent.click(screen.getByRole("button", { name: "Отклонить" }));
  fireEvent.change(await screen.findByLabelText("Причина отклонения"), { target: { value: "не по теме" } });
  fireEvent.click(screen.getByRole("button", { name: "Отклонить письмо" }));
  await waitFor(() => expect(sent(calls, "POST", `${BASE}/moderation/h2/reject`)[0]?.body).toEqual({ reason: "не по теме" }));
  await waitFor(() => expect(screen.queryByText("h2@ext.example")).toBeNull());
});

test("очередь модерации: тема письма — только по переключателю (include=subject); оператору переключателя нет", async () => {
  const calls = open("/moderation", {
    [`GET ${BASE}/moderation`]: (c) => ({ body: [held("h1", c.path.includes("include=subject") ? { subject: "Отчёт за ноябрь" } : {})] }),
    [`GET ${BASE}/groups`]: { body: [group("g1", "Staff")] },
  });
  expect(await screen.findByText("h1@ext.example")).toBeTruthy();
  expect(screen.queryByText("Тема")).toBeNull();
  fireEvent.click(await screen.findByRole("switch", { name: "Показать темы писем" }));
  expect(await screen.findByText("Отчёт за ноябрь")).toBeTruthy();
  expect(sent(calls, "GET", `${BASE}/moderation?include=subject`)).toHaveLength(1);
  cleanup();

  const read = open("/moderation", { [`GET ${BASE}/moderation`]: { body: [held("h2")] }, [`GET ${BASE}/groups`]: { body: [] } }, ["operator"]);
  expect(await screen.findByText("h2@ext.example")).toBeTruthy();
  await waitFor(() => expect(read.some((c) => c.path === "/api/v1/whoami")).toBe(true));
  expect(screen.queryByRole("switch", { name: "Показать темы писем" })).toBeNull();
  expect(read.some((c) => c.path.includes("include="))).toBe(false);
});
