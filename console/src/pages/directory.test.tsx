// Экраны операций ADM-10 (UI-17): адресная книга, оргструктура, отключённые ящики, аудит ящиков,
// остановка синхронизации каталога.
import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, expect, test } from "vitest";

import { allowed } from "../providers/access";
import { ORG } from "../test-api";
import { BASE, buttons, open, sent } from "../test-app";

import { toBody, toForm } from "./contacts";
import { tree } from "./units";

afterEach(() => {
  cleanup();
  sessionStorage.clear();
  localStorage.clear();
});

const contact = (id: string, email: string, more: object = {}) => ({
  id,
  display_name: `Контакт ${id}`,
  email,
  attributes: {},
  groups: [],
  hidden_from_gal: false,
  external_id: null,
  ...more,
});
const group = (id: string, name: string) => ({
  id,
  name,
  address: null,
  external_id: null,
  hidden_from_gal: false,
  members: [],
  moderators: [],
  senders: "anyone",
  names: {},
});
const empty = { quota_bytes: null };

test("контакт ↔ форма: атрибуты — пары, атрибут без имени не отправляется", () => {
  const form = toForm(contact("c1", "p@example.com", { attributes: { department: "Продажи" }, groups: ["g1"] }) as never);
  expect(form.attributes).toEqual([{ name: "department", value: "Продажи" }]);
  expect(toBody({ ...form, display_name: " Партнёр ", attributes: [...form.attributes, { name: " ", value: "x" }] })).toEqual({
    display_name: "Партнёр",
    email: "p@example.com",
    groups: ["g1"],
    hidden_from_gal: false,
    attributes: { department: "Продажи" },
  });
});

test("адресная книга: список с группами, создание — POST, контакт каталога удалить нельзя", async () => {
  const calls = open("/contacts", {
    [`GET ${BASE}/contacts`]: {
      body: [contact("c1", "partner@example.com", { groups: ["g1"] }), contact("c2", "dir@example.com", { external_id: "cn=c2", hidden_from_gal: true })],
    },
    [`GET ${BASE}/groups`]: { body: [group("g1", "Партнёры")] },
    [`POST ${BASE}/contacts`]: (c) => ({ status: 201, body: { id: "c9", external_id: null, ...(c.body as object) } }),
  });
  expect(await screen.findByText("partner@example.com")).toBeTruthy();
  expect(await screen.findByText("Партнёры")).toBeTruthy();
  expect(screen.getByText("скрыт из адресной книги")).toBeTruthy();
  await waitFor(() => {
    const remove = screen.getAllByRole("button", { name: /Удалить/ }) as HTMLButtonElement[];
    expect(remove.map((b) => b.disabled)).toEqual([false, true]);
  });
  fireEvent.click((await buttons(/Создать/))[0]);
  const [save] = await buttons(/Сохранить/);
  fireEvent.change(await screen.findByLabelText("Отображаемое имя"), { target: { value: "Новый партнёр" } });
  fireEvent.change(screen.getByLabelText("Адрес"), { target: { value: "new@example.com" } });
  fireEvent.click(screen.getByRole("button", { name: "Добавить атрибут" }));
  fireEvent.change(await screen.findByLabelText("Имя атрибута"), { target: { value: "company" } });
  fireEvent.change(screen.getByLabelText("Значение атрибута"), { target: { value: "ООО Ромашка" } });
  fireEvent.click(save);
  await waitFor(() =>
    expect(sent(calls, "POST", `${BASE}/contacts`)[0]?.body).toEqual({
      display_name: "Новый партнёр",
      email: "new@example.com",
      groups: [],
      hidden_from_gal: false,
      attributes: { company: "ООО Ромашка" },
    }),
  );
});

test("контакт: изменение — PUT целиком; контакт каталога только для просмотра", async () => {
  const calls = open("/contacts/c1", {
    [`GET ${BASE}/contacts/c1`]: { body: contact("c1", "partner@example.com") },
    [`GET ${BASE}/groups`]: { body: [] },
    [`PUT ${BASE}/contacts/c1`]: (c) => ({ body: { id: "c1", external_id: null, ...(c.body as object) } }),
  });
  const name = (await screen.findByLabelText("Отображаемое имя")) as HTMLInputElement;
  await waitFor(() => expect(name.value).toBe("Контакт c1"));
  const [save] = await buttons(/Сохранить/);
  fireEvent.change(screen.getByLabelText("Отображаемое имя"), { target: { value: "Партнёр" } });
  fireEvent.click(save);
  await waitFor(() =>
    expect(sent(calls, "PUT", `${BASE}/contacts/c1`)[0]?.body).toEqual({
      display_name: "Партнёр",
      email: "partner@example.com",
      groups: [],
      hidden_from_gal: false,
      attributes: {},
    }),
  );
  expect(await screen.findByText("Контакт сохранён")).toBeTruthy();
  cleanup();

  open("/contacts/c2", {
    [`GET ${BASE}/contacts/c2`]: { body: contact("c2", "dir@example.com", { external_id: "cn=c2" }) },
    [`GET ${BASE}/groups`]: { body: [] },
  });
  expect(await screen.findByText("Контакт каталога: его ведёт каталог.")).toBeTruthy();
  expect(((await screen.findByLabelText("Адрес")) as HTMLInputElement).disabled).toBe(true);
});

test("оргструктура: дерево по parent, новое подразделение, сотрудники с вложенными", async () => {
  expect(
    tree([
      { id: "b", name: "Продажи", parent: "a" },
      { id: "a", name: "Головной офис", parent: null },
      { id: "x", name: "Сирота", parent: "нет" },
    ]).map((n) => [n.name, n.children?.map((c) => c.name)]),
  ).toEqual([
    ["Головной офис", ["Продажи"]],
    ["Сирота", undefined],
  ]);
  const calls = open("/units", {
    [`GET ${BASE}/units`]: {
      body: [
        { id: "a", name: "Головной офис", parent: null, external_id: null },
        { id: "b", name: "Продажи", parent: "a", external_id: "ou=sales" },
      ],
    },
    [`POST ${BASE}/units`]: (c) => ({ status: 201, body: { id: "c", external_id: null, ...(c.body as object) } }),
    [`GET ${BASE}/units/a/members`]: (c) => ({
      body: c.path.includes("subtree=true")
        ? [{ account: "a1", title: "Директор", seniority: 10 }, { account: "a2", department: "Продажи" }]
        : [{ account: "a1", title: "Директор", seniority: 10 }],
    }),
  });
  expect(await screen.findByText("Продажи")).toBeTruthy();
  // Подразделение каталога не меняется и не удаляется.
  await waitFor(() => expect(screen.getAllByRole("button", { name: "Изменить" })).toHaveLength(1));

  fireEvent.click(await screen.findByRole("button", { name: "Новое подразделение" }));
  fireEvent.change(await screen.findByLabelText("Название"), { target: { value: " Склад " } });
  fireEvent.click(screen.getByRole("button", { name: "Сохранить" }));
  await waitFor(() => expect(sent(calls, "POST", `${BASE}/units`)[0]?.body).toEqual({ name: "Склад", parent: null }));

  fireEvent.click(screen.getAllByRole("button", { name: "Сотрудники" })[0]);
  expect(await screen.findByText("Директор")).toBeTruthy();
  fireEvent.click(screen.getByRole("switch", { name: "С вложенными подразделениями" }));
  expect(await screen.findByText("a2")).toBeTruthy();
  expect(sent(calls, "GET", `${BASE}/units/a/members?subtree=true`)).toHaveLength(1);
});

test("отключённые ящики: восстановление и подключение к учётной записи", async () => {
  let boxes = [
    { id: "d1", login: "old@example.org", display_name: "Старый", external_id: "uid=old", until: "2026-12-01T00:00:00Z" },
    { id: "d2", login: "gone@example.org", display_name: "Ушедший", external_id: null, until: "2026-12-05T00:00:00Z" },
  ];
  const done = (id: string, status = 204) => () => {
    boxes = boxes.filter((b) => b.id !== id);
    return { status, body: status === 204 ? undefined : { id } };
  };
  const calls = open("/disabled-accounts", {
    [`GET ${BASE}/disabled-accounts`]: () => ({ body: boxes }),
    [`GET ${BASE}/accounts`]: { body: [{ id: "a5", login: "new@example.org" }] },
    [`POST ${BASE}/disabled-accounts/d1/restore`]: done("d1"),
    [`POST ${BASE}/disabled-accounts/d2/attach`]: done("d2", 200),
  });
  expect(await screen.findByText("old@example.org")).toBeTruthy();
  fireEvent.click((await screen.findAllByRole("button", { name: "Восстановить" }))[0]);
  const confirm = await screen.findByText("Восстановить ящик old@example.org?");
  fireEvent.click(within(confirm.closest(".ant-popover") as HTMLElement).getByRole("button", { name: "Восстановить" }));
  await waitFor(() => expect(sent(calls, "POST", `${BASE}/disabled-accounts/d1/restore`)).toHaveLength(1));
  await waitFor(() => expect(screen.queryByText("old@example.org")).toBeNull());

  fireEvent.click(screen.getByRole("button", { name: "Подключить…" }));
  fireEvent.mouseDown(await screen.findByLabelText("Учётная запись"));
  fireEvent.click(await screen.findByTitle("new@example.org"));
  fireEvent.click(screen.getByRole("button", { name: "Подключить" }));
  await waitFor(() => expect(sent(calls, "POST", `${BASE}/disabled-accounts/d2/attach`)[0]?.body).toEqual({ to: "a5", existing: null }));
  expect(await screen.findByText("Ящик подключён")).toBeTruthy();
});

test("аудит ящиков: выборка по ящику и действию; политика организации — текущая, PUT и сброс", async () => {
  const entry = { account: "a1", at: "2026-11-10T10:00:00Z", actor: "boss@example.org", actor_kind: "delegate", action: "send_as" };
  const calls = open("/mailbox-audit?account=a1", {
    [`GET ${BASE}/mailbox-audit`]: { body: { entries: [entry], next: null } },
    [`GET ${BASE}/mailbox-audit/policy`]: { body: { non_owner: false, owner: ["send"] } },
    [`PUT ${BASE}/mailbox-audit/policy`]: (c) => ({ body: c.body }),
    [`DELETE ${BASE}/mailbox-audit/policy`]: { status: 204 },
  });
  expect(await screen.findByText("boss@example.org")).toBeTruthy();
  expect(screen.getByText("представитель")).toBeTruthy();
  expect(screen.getByText("отправка от имени (send as)")).toBeTruthy();
  expect(sent(calls, "GET", `${BASE}/mailbox-audit?account=a1`)).toHaveLength(1);

  fireEvent.change(screen.getByLabelText("Исполнитель"), { target: { value: " boss@example.org " } });
  fireEvent.click(screen.getByRole("button", { name: "Найти" }));
  await waitFor(() => expect(sent(calls, "GET", `${BASE}/mailbox-audit?account=a1&actor=boss%40example.org`)).toHaveLength(1));

  // Форма открывается с текущей политикой организации, а не со значениями по умолчанию.
  expect(((await screen.findByLabelText("отправка")) as HTMLInputElement).checked).toBe(true);
  expect(screen.getByRole("switch").getAttribute("aria-checked")).toBe("false");
  fireEvent.click(screen.getByLabelText("удаление писем"));
  fireEvent.click(screen.getByRole("button", { name: "Сохранить политику аудита" }));
  await waitFor(() =>
    expect(sent(calls, "PUT", `${BASE}/mailbox-audit/policy`)[0]?.body).toEqual({ non_owner: false, owner: ["delete", "send"] }),
  );
  fireEvent.click(screen.getByRole("button", { name: "Сбросить к политике по умолчанию" }));
  const confirm = await screen.findByText("Сбросить политику аудита организации?");
  fireEvent.click(within(confirm.closest(".ant-popover") as HTMLElement).getByRole("button", { name: "Сбросить" }));
  await waitFor(() => expect(sent(calls, "DELETE", `${BASE}/mailbox-audit/policy`)).toHaveLength(1));
});

test("аудитор: аудит ящиков и политика организации — только просмотр, оператору — «Нет доступа»", async () => {
  open(
    "/mailbox-audit",
    {
      [`GET ${BASE}/mailbox-audit`]: { body: { entries: [], next: null } },
      [`GET ${BASE}/mailbox-audit/policy`]: { body: { non_owner: true, owner: ["delete"] } },
    },
    ["auditor"],
  );
  expect(await screen.findByText("Аудит ящиков")).toBeTruthy();
  expect(await screen.findByText("Политика аудита организации")).toBeTruthy();
  const del = (await screen.findByLabelText("удаление писем")) as HTMLInputElement;
  expect(del.checked && del.disabled).toBe(true);
  expect(screen.queryByRole("button", { name: "Сохранить политику аудита" })).toBeNull();
  cleanup();
  open("/mailbox-audit", {}, ["operator"]);
  expect(await screen.findByText("Нет доступа")).toBeTruthy();

  expect(allowed(["auditor"], "mailbox-audit", "list")).toBe(true);
  expect(allowed(["auditor"], "mailbox-audit", "edit")).toBe(false);
  expect(allowed(["operator"], "mailbox-audit", "list")).toBe(false);
  expect(allowed(["organization_admin"], "mailbox-audit", "edit")).toBe(true);
});

test("организация: остановленная синхронизация каталога — подтверждение; без остановки предупреждения нет", async () => {
  let approved = false;
  const org = { id: ORG, default: true, policy: empty, effective: empty };
  const hold = () => ({ body: { removed: 40, total: 100, detected_at: "2026-11-12T08:00:00Z", approved } });
  const calls = open("/organization", {
    [`GET ${BASE}`]: { body: org },
    [`GET ${BASE}/sync-hold`]: hold,
    [`POST ${BASE}/sync-hold/approve`]: () => {
      approved = true;
      return hold();
    },
  });
  expect(await screen.findByText(/Синхронизация каталога остановлена: к отключению 40 из 100/)).toBeTruthy();
  fireEvent.click(await screen.findByRole("button", { name: "Подтвердить отключение" }));
  const confirm = await screen.findByText("Подтвердить отключение 40 учётных записей?");
  fireEvent.click(within(confirm.closest(".ant-popover") as HTMLElement).getByRole("button", { name: "Подтвердить" }));
  await waitFor(() => expect(sent(calls, "POST", `${BASE}/sync-hold/approve`)).toHaveLength(1));
  expect(await screen.findByText(/Отключение подтверждено: его выполнит следующая синхронизация/)).toBeTruthy();
  cleanup();

  open("/organization", { [`GET ${BASE}`]: { body: org } });
  expect(await screen.findByText("Политика организации")).toBeTruthy();
  expect(screen.queryByText(/Синхронизация каталога/)).toBeNull();
  // 404 остановки — не ошибка экрана: уведомления нет.
  expect(screen.queryByText("Not Found")).toBeNull();
});
