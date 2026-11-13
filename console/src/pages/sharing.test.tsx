import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, expect, test } from "vitest";

import { NO_RIGHTS, fromRights, toRights } from "../components/rights";
import { BASE, open, sent } from "../test-app";

const empty = {
  archive_after: null,
  archive_quota_bytes: null,
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
const account = {
  id: "a1",
  login: "alice@example.org",
  domain: "d1",
  display_name: "Алиса",
  enabled: true,
  external_id: null,
  quota: { used_bytes: 0, messages: 0, limit_bytes: null },
  policy: empty,
  effective: empty,
};
const delegate = (level: "read" | "write") => ({
  delegate: "a2",
  login: "aide@example.org",
  display_name: "Помощник",
  level,
  schedule_on_behalf: false,
});

afterEach(() => {
  cleanup();
  sessionStorage.clear();
  localStorage.clear();
});

test("права ↔ флажки", () => {
  expect(fromRights(toRights(["read", "schedule"]))).toEqual(["read", "schedule"]);
  expect(toRights()).toEqual(NO_RIGHTS);
});

test("представители: назначение — PUT, смена уровня, отзыв — DELETE", async () => {
  let list = [delegate("read")];
  const calls = open("/accounts/a1", {
    [`GET ${BASE}/accounts/a1`]: { body: account },
    [`GET ${BASE}/accounts/a1/app-passwords`]: { body: [] },
    [`GET ${BASE}/accounts/a1/collections`]: { body: [] },
    [`GET ${BASE}/accounts`]: { body: [{ id: "a3", login: "new@example.org" }] },
    [`GET ${BASE}/accounts/a1/delegates`]: () => ({ body: list }),
    [`PUT ${BASE}/accounts/a1/delegates/a3`]: (c) => ({ body: { ...delegate("read"), delegate: "a3", ...(c.body as object) } }),
    [`PUT ${BASE}/accounts/a1/delegates/a2`]: (c) => {
      list = [{ ...delegate("read"), ...(c.body as object) }];
      return { body: list[0] };
    },
    [`DELETE ${BASE}/accounts/a1/delegates/a2`]: () => {
      list = [];
      return { status: 204 };
    },
  });
  expect(await screen.findByText("aide@example.org")).toBeTruthy();
  expect(await screen.findByText("Календарей и адресных книг нет")).toBeTruthy();

  fireEvent.mouseDown(await screen.findByLabelText("Представитель"));
  fireEvent.click(await screen.findByTitle("new@example.org"));
  fireEvent.click(screen.getByRole("switch", { name: "Приглашения от имени" }));
  fireEvent.click(screen.getByRole("button", { name: "Назначить" }));
  await waitFor(() =>
    expect(sent(calls, "PUT", `${BASE}/accounts/a1/delegates/a3`)[0]?.body).toEqual({ level: "read", schedule_on_behalf: true }),
  );
  expect(await screen.findByText("Представитель назначен")).toBeTruthy();

  fireEvent.mouseDown(screen.getByRole("combobox", { name: "Доступ aide@example.org" }));
  fireEvent.click(await screen.findByTitle("чтение и запись"));
  await waitFor(() =>
    expect(sent(calls, "PUT", `${BASE}/accounts/a1/delegates/a2`)[0]?.body).toEqual({ level: "write", schedule_on_behalf: false }),
  );

  fireEvent.click(await screen.findByRole("button", { name: "Отозвать" }));
  const confirm = await screen.findByText("Отозвать представителя aide@example.org?");
  fireEvent.click(within(confirm.closest(".ant-popover") as HTMLElement).getByRole("button", { name: "Отозвать" }));
  await waitFor(() => expect(sent(calls, "DELETE", `${BASE}/accounts/a1/delegates/a2`)).toHaveLength(1));
  await waitFor(() => expect(screen.queryByText("aide@example.org")).toBeNull());
});

test("права на календарь: список коллекций, выдача — PUT acl/{identifier}, отзыв — пустые права, отказ политики", async () => {
  let entries = [{ identifier: "anyone", rights: { ...NO_RIGHTS, read: true } }];
  const acl = `${BASE}/accounts/a1/collections/c1/acl`;
  const calls = open("/accounts/a1", {
    [`GET ${BASE}/accounts/a1`]: { body: account },
    [`GET ${BASE}/accounts/a1/app-passwords`]: { body: [] },
    [`GET ${BASE}/accounts/a1/delegates`]: { body: [] },
    [`GET ${BASE}/accounts/a1/collections`]: {
      body: [
        { id: "c1", kind: "calendar", name: "work", display_name: "Работа" },
        { id: "c2", kind: "address_book", name: "contacts", display_name: "Контакты" },
      ],
    },
    [`GET ${acl}`]: () => ({ body: entries }),
    [`PUT ${acl}/peer%40example.org`]: (c) => {
      entries = [...entries, { identifier: "peer@example.org", rights: c.body as typeof NO_RIGHTS }];
      return { body: entries };
    },
    [`PUT ${acl}/out%40example.org`]: {
      status: 409,
      body: { status: 409, title: "Conflict", detail: "политика общего доступа организации не допускает" },
    },
    [`PUT ${acl}/anyone`]: (c) => {
      entries = entries.filter((e) => e.identifier !== "anyone");
      return { body: c.body && entries };
    },
  });
  expect(await screen.findByText("все (anyone)")).toBeTruthy();
  expect(screen.getByText("Календарь: Работа (work)")).toBeTruthy();

  fireEvent.change(screen.getByRole("combobox", { name: "Кому" }), { target: { value: "peer@example.org" } });
  fireEvent.click(screen.getByLabelText("запись"));
  fireEvent.click(screen.getByRole("button", { name: "Выдать" }));
  await waitFor(() => expect(sent(calls, "PUT", `${acl}/peer%40example.org`)[0]?.body).toEqual({ ...NO_RIGHTS, read: true, write: true }));
  expect(await screen.findByText("peer@example.org")).toBeTruthy();

  fireEvent.change(screen.getByRole("combobox", { name: "Кому" }), { target: { value: "out@example.org" } });
  fireEvent.click(screen.getByRole("button", { name: "Выдать" }));
  expect(await screen.findByText("политика общего доступа организации не допускает")).toBeTruthy();

  fireEvent.click((await screen.findAllByRole("button", { name: "Отозвать" }))[0]);
  const confirm = await screen.findByText("Отозвать права anyone?");
  fireEvent.click(within(confirm.closest(".ant-popover") as HTMLElement).getByRole("button", { name: "Отозвать" }));
  await waitFor(() => expect(sent(calls, "PUT", `${acl}/anyone`)[0]?.body).toEqual(NO_RIGHTS));
  await waitFor(() => expect(screen.queryByText("все (anyone)")).toBeNull());
});

test("политика общего доступа организации: текущая, группы — PUT calendar-sharing", async () => {
  const calls = open("/organization", {
    [`GET ${BASE}`]: { body: { id: "o", default: true, policy: empty, effective: empty } },
    [`GET ${BASE}/groups`]: { body: [{ id: "g1", name: "Руководство" }] },
    [`GET ${BASE}/calendar-sharing`]: { body: { scope: { type: "organization" }, max_outside: { ...NO_RIGHTS, free_busy: true, read: true } } },
    [`PUT ${BASE}/calendar-sharing`]: (c) => ({ body: c.body }),
  });
  const groups = await screen.findByLabelText("полные права — участникам групп, остальным — ограниченные");
  expect((screen.getByLabelText("любому субъекту организации") as HTMLInputElement).checked).toBe(true);
  fireEvent.click(groups);
  fireEvent.mouseDown(await screen.findByLabelText("Группы с полными правами"));
  fireEvent.click(await screen.findByTitle("Руководство"));
  fireEvent.click(screen.getByLabelText("чтение"));
  fireEvent.click(screen.getByRole("button", { name: "Сохранить политику общего доступа" }));
  await waitFor(() =>
    expect(sent(calls, "PUT", `${BASE}/calendar-sharing`)[0]?.body).toEqual({
      scope: { type: "groups", groups: ["g1"] },
      max_outside: { ...NO_RIGHTS, free_busy: true },
    }),
  );
  expect(await screen.findByText("Политика общего доступа сохранена")).toBeTruthy();
});
