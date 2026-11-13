import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, expect, test } from "vitest";

import { BASE, buttons, open, sent } from "../test-app";

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
const account = (id: string, login: string, more: object = {}) => ({
  id,
  login,
  domain: "d1",
  display_name: login,
  enabled: true,
  external_id: null,
  aliases: [] as string[],
  hidden_from_gal: false,
  quota: { used_bytes: 2048, messages: 3, limit_bytes: null },
  policy: empty,
  effective: empty,
  ...more,
});

afterEach(() => {
  cleanup();
  sessionStorage.clear();
  localStorage.clear();
});

test("учётные записи: страницы по курсору из Link, поиск по началу имени входа", async () => {
  const calls = open("/accounts", {
    [`GET ${BASE}/accounts`]: (c) => {
      const q = new URL(c.path, "http://x").searchParams;
      if (q.get("prefix") === "bo") return { body: [account("a3", "bob@example.org")] };
      if (q.get("after") === "alice@example.org") return { body: [account("a2", "carol@example.org", { enabled: false })] };
      return {
        body: [account("a1", "alice@example.org", { external_id: "uid=alice" })],
        headers: { Link: `<${BASE}/accounts?limit=1&after=alice%40example.org>; rel="next"` },
      };
    },
  });
  expect((await screen.findAllByText("alice@example.org")).length).toBeGreaterThan(0);
  expect(screen.getByText("каталог")).toBeTruthy();
  expect(screen.getByText("2.0 КБ")).toBeTruthy();
  // Размер страницы задаёт узел: первая страница — без параметров.
  expect(sent(calls, "GET", `${BASE}/accounts`)).toHaveLength(1);

  fireEvent.click(screen.getByRole("button", { name: "Показать ещё" }));
  expect((await screen.findAllByText("carol@example.org")).length).toBeGreaterThan(0);
  expect(screen.getByText("Заблокирована")).toBeTruthy();
  expect(sent(calls, "GET", `${BASE}/accounts?after=alice%40example.org`)).toHaveLength(1);
  // Последняя страница — без Link: кнопки продолжения нет.
  await waitFor(() => expect(screen.queryByRole("button", { name: "Показать ещё" })).toBeNull());

  fireEvent.change(screen.getByLabelText("Поиск по имени входа"), { target: { value: " bo " } });
  fireEvent.click(screen.getByRole("button", { name: "Найти" }));
  expect((await screen.findAllByText("bob@example.org")).length).toBeGreaterThan(0);
  expect(sent(calls, "GET", `${BASE}/accounts?prefix=bo`)).toHaveLength(1);
  expect(screen.queryByText("alice@example.org")).toBeNull();
});

test("учётная запись: создание; ошибка API — в уведомлении", async () => {
  const calls = open("/accounts/create", {
    [`POST ${BASE}/accounts`]: { status: 409, body: { status: 409, title: "Conflict", detail: "имя занято" } },
  });
  // Форма готова, когда известны роли (кнопка включена): раньше введённое сбрасывается при её подготовке.
  const [save] = await buttons(/Сохранить/);
  fireEvent.change(await screen.findByLabelText("Имя входа"), { target: { value: "dave@example.org" } });
  fireEvent.change(screen.getByLabelText("Отображаемое имя"), { target: { value: "Дэйв" } });
  fireEvent.click(save);
  await waitFor(() =>
    expect(sent(calls, "POST", `${BASE}/accounts`)[0]?.body).toEqual({ login: "dave@example.org", display_name: "Дэйв" }),
  );
  expect(await screen.findByText("имя занято")).toBeTruthy();
});

test("учётная запись: политика ящика — PUT accounts/{account}/policy", async () => {
  const calls = open("/accounts/a1", {
    [`GET ${BASE}/accounts/a1`]: {
      body: account("a1", "alice@example.org", { policy: { ...empty, quota_bytes: 100 }, effective: { ...empty, quota_bytes: 100 } }),
    },
    [`GET ${BASE}/accounts/a1/app-passwords`]: { body: [] },
    [`GET ${BASE}/transport/domains`]: {
      body: { version: 4, domains: [{ id: "d1", tenant: "t", name: "example.org", kind: "authoritative" }] },
    },
    [`PUT ${BASE}/accounts/a1/policy`]: (c) => ({ body: c.body }),
  });
  await waitFor(() => expect((screen.getByLabelText("Квота ящика, байт.") as HTMLInputElement).value).toBe("100"));
  expect(screen.getByText("Вход разрешён")).toBeTruthy();
  // Основной домен — по имени из принятых доменов транспорта, а не идентификатором.
  expect(await screen.findByText("example.org")).toBeTruthy();
  expect(sent(calls, "GET", `${BASE}/transport/domains`).length).toBeGreaterThan(0);
  expect(calls.some((c) => c.path.startsWith(`${BASE}/domains`))).toBe(false);
  fireEvent.change(screen.getByLabelText("Квота ящика, байт."), { target: { value: "200" } });
  fireEvent.click(screen.getByRole("button", { name: "Сохранить политику" }));
  await waitFor(() => expect(sent(calls, "PUT", `${BASE}/accounts/a1/policy`)[0]?.body).toEqual({ ...empty, quota_bytes: 200 }));
  expect(await screen.findByText("Политика ящика сохранена")).toBeTruthy();
});

test("учётная запись: блокировка и имя — PATCH только изменённых полей; запись каталога не меняется", async () => {
  let current = account("a1", "alice@example.org");
  const calls = open("/accounts/a1", {
    [`GET ${BASE}/accounts/a1`]: () => ({ body: current }),
    [`GET ${BASE}/accounts/a1/app-passwords`]: { body: [] },
    [`PATCH ${BASE}/accounts/a1`]: (c) => {
      current = { ...current, ...(c.body as object) };
      return { body: current };
    },
  });
  const save = await waitFor(() => {
    const b = screen.getByRole("button", { name: "Сохранить" }) as HTMLButtonElement;
    expect(b.disabled).toBe(true);
    return b;
  });
  fireEvent.click(screen.getByRole("switch", { name: "Разрешить вход" }));
  await waitFor(() => expect(save.disabled).toBe(false));
  fireEvent.click(save);
  await waitFor(() => expect(sent(calls, "PATCH", `${BASE}/accounts/a1`)[0]?.body).toEqual({ enabled: false }));
  expect(await screen.findByText("Учётная запись заблокирована")).toBeTruthy();
  expect(await screen.findByText("Заблокирована")).toBeTruthy();

  fireEvent.change(screen.getByRole("textbox", { name: "Отображаемое имя" }), { target: { value: " Алиса " } });
  fireEvent.click(screen.getByRole("button", { name: "Сохранить" }));
  await waitFor(() => expect(sent(calls, "PATCH", `${BASE}/accounts/a1`)[1]?.body).toEqual({ display_name: "Алиса" }));
});

test("учётная запись каталога: имя и блокировка только для чтения, псевдонимов и скрытия в форме нет", async () => {
  open("/accounts/a1", {
    [`GET ${BASE}/accounts/a1`]: {
      body: account("a1", "alice@example.org", { external_id: "uid=alice", aliases: ["a@example.org"], hidden_from_gal: true }),
    },
    [`GET ${BASE}/accounts/a1/app-passwords`]: { body: [] },
  });
  expect(
    await screen.findByText("Имя, блокировку, псевдонимы и скрытие из книги «Организация» записи каталога ведёт каталог."),
  ).toBeTruthy();
  expect((screen.getByRole("switch", { name: "Разрешить вход" }) as HTMLButtonElement).disabled).toBe(true);
  expect(screen.queryByRole("button", { name: "Сохранить" })).toBeNull();
  // Значения каталога — в сведениях карточки, полей формы нет.
  expect(screen.getByText("a@example.org")).toBeTruthy();
  expect(screen.getByText("скрыта")).toBeTruthy();
  expect(screen.queryByRole("switch", { name: "Скрыть из книги «Организация»" })).toBeNull();
  expect(screen.queryByLabelText("Псевдонимы")).toBeNull();
});

test("локальная учётная запись: псевдонимы и скрытие из книги — PATCH только изменённых полей", async () => {
  let current = account("a1", "alice@example.org", { aliases: ["al@example.org"] });
  const calls = open("/accounts/a1", {
    [`GET ${BASE}/accounts/a1`]: () => ({ body: current }),
    [`GET ${BASE}/accounts/a1/app-passwords`]: { body: [] },
    [`PATCH ${BASE}/accounts/a1`]: (c) => {
      current = { ...current, ...(c.body as object) };
      return { body: current };
    },
  });
  const save = await waitFor(() => {
    const b = screen.getByRole("button", { name: "Сохранить" }) as HTMLButtonElement;
    expect(b.disabled).toBe(true);
    return b;
  });
  expect(screen.getByText("видна")).toBeTruthy();
  fireEvent.change(screen.getByLabelText("Псевдонимы"), { target: { value: "alice.s@example.org," } });
  fireEvent.click(screen.getByRole("switch", { name: "Скрыть из книги «Организация»" }));
  await waitFor(() => expect(save.disabled).toBe(false));
  fireEvent.click(save);
  await waitFor(() =>
    expect(sent(calls, "PATCH", `${BASE}/accounts/a1`)[0]?.body).toEqual({
      aliases: ["al@example.org", "alice.s@example.org"],
      hidden_from_gal: true,
    }),
  );
  expect(await screen.findByText("скрыта")).toBeTruthy();
  expect(screen.getByText("al@example.org, alice.s@example.org")).toBeTruthy();
});

test("пароли приложений: выпуск показывает пароль один раз, отзыв — DELETE", async () => {
  const passwords = [
    { id: "p1", label: "Телефон", created_at: "2026-11-01T10:00:00Z", revoked_at: null },
    { id: "p0", label: "Старый", created_at: "2026-10-01T10:00:00Z", revoked_at: "2026-10-02T10:00:00Z" },
  ];
  const calls = open("/accounts/a1", {
    [`GET ${BASE}/accounts/a1`]: { body: account("a1", "alice@example.org") },
    [`GET ${BASE}/accounts/a1/app-passwords`]: { body: passwords },
    [`POST ${BASE}/accounts/a1/app-passwords`]: {
      status: 201,
      body: { id: "p2", label: "Ноутбук", created_at: "2026-11-12T10:00:00Z", revoked_at: null, password: "abcd-efgh-ijkl" },
    },
    [`DELETE ${BASE}/accounts/a1/app-passwords/p1`]: { status: 204 },
  });
  expect(await screen.findByText("Телефон")).toBeTruthy();
  // Отозванный пароль виден, но отзывать его нельзя.
  expect(screen.getByText("Старый")).toBeTruthy();
  expect(screen.getAllByRole("button", { name: "Отозвать" })).toHaveLength(1);

  fireEvent.change(screen.getByLabelText("Подпись"), { target: { value: "Ноутбук" } });
  fireEvent.click(screen.getByRole("button", { name: "Выпустить пароль" }));
  expect(await screen.findByText("abcd-efgh-ijkl")).toBeTruthy();
  expect(sent(calls, "POST", `${BASE}/accounts/a1/app-passwords`)[0]?.body).toEqual({ label: "Ноутбук" });
  fireEvent.click(screen.getByRole("button", { name: "Закрыть" }));
  await waitFor(() => expect(screen.queryByText("abcd-efgh-ijkl")).toBeNull());

  fireEvent.click(screen.getByRole("button", { name: "Отозвать" }));
  expect(await screen.findByText("Отозвать пароль «Телефон»?")).toBeTruthy();
  const buttons = screen.getAllByRole("button", { name: "Отозвать" });
  fireEvent.click(buttons[buttons.length - 1]);
  await waitFor(() => expect(sent(calls, "DELETE", `${BASE}/accounts/a1/app-passwords/p1`)).toHaveLength(1));
});

test("учётная запись: списки отправителей — PUT целиком; аудит ящика — политика, сброс, ссылка на журнал", async () => {
  let policy = { non_owner: true, owner: ["send"] };
  const calls = open("/accounts/a1", {
    [`GET ${BASE}/accounts/a1`]: { body: account("a1", "alice@example.org") },
    [`GET ${BASE}/accounts/a1/app-passwords`]: { body: [] },
    [`GET ${BASE}/accounts/a1/senders`]: { body: { safe: ["partner.example"], blocked: [] } },
    [`PUT ${BASE}/accounts/a1/senders`]: (c) => ({ body: c.body }),
    [`GET ${BASE}/accounts/a1/audit-policy`]: () => ({ body: policy }),
    [`PUT ${BASE}/accounts/a1/audit-policy`]: (c) => {
      policy = c.body as typeof policy;
      return { body: policy };
    },
    [`DELETE ${BASE}/accounts/a1/audit-policy`]: () => {
      policy = { non_owner: true, owner: [] };
      return { status: 204 };
    },
  });
  expect(await screen.findByText("partner.example")).toBeTruthy();
  // Адреса вводятся через запятую или пробел.
  fireEvent.change(screen.getByLabelText("Заблокированные отправители"), { target: { value: "spam.example," } });
  fireEvent.click(screen.getByRole("button", { name: "Сохранить списки" }));
  await waitFor(() =>
    expect(sent(calls, "PUT", `${BASE}/accounts/a1/senders`)[0]?.body).toEqual({ safe: ["partner.example"], blocked: ["spam.example"] }),
  );

  const send = (await screen.findByLabelText("отправка")) as HTMLInputElement;
  expect(send.checked).toBe(true);
  fireEvent.click(screen.getByLabelText("удаление писем"));
  fireEvent.click(screen.getByRole("button", { name: "Сохранить политику аудита" }));
  await waitFor(() =>
    expect(sent(calls, "PUT", `${BASE}/accounts/a1/audit-policy`)[0]?.body).toEqual({ non_owner: true, owner: ["delete", "send"] }),
  );
  fireEvent.click(screen.getByRole("button", { name: "Сбросить к политике организации" }));
  const confirm = await screen.findByText(/Сбросить политику аудита ящика\?/);
  fireEvent.click(within(confirm.closest(".ant-popover") as HTMLElement).getByRole("button", { name: "Сбросить" }));
  await waitFor(() => expect(sent(calls, "DELETE", `${BASE}/accounts/a1/audit-policy`)).toHaveLength(1));
  await waitFor(() => expect((screen.getByLabelText("отправка") as HTMLInputElement).checked).toBe(false));
  expect(screen.getByRole("link", { name: "Журнал аудита ящика" }).getAttribute("href")).toBe("/mailbox-audit?account=a1");
});
