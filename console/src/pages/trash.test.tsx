import { cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, test } from "vitest";

import { BASE, open, sent } from "../test-app";

const A = `${BASE}/accounts/a1`;
const entry = (message: string, origin = "INBOX") => ({
  message,
  origin,
  size: 2048,
  received_at: "2026-11-01T10:00:00Z",
  deleted_at: "2026-11-10T10:00:00Z",
  expires_at: "2026-12-10T10:00:00Z",
});
const policy = { trash_retention: null };
const routes = {
  [`GET ${A}`]: { body: { id: "a1", login: "alice@example.org", policy, effective: { trash_retention: "30d" } } },
  [`GET ${A}/trash-purge`]: { body: { rule: { mode: "hide", user_can_choose: true }, user: "erase", effective: "erase" } },
};

afterEach(() => {
  cleanup();
  sessionStorage.clear();
  localStorage.clear();
});

test("корзина: правило trash.purge, страницы по next_offset, восстановление", async () => {
  let restored = false;
  const calls = open("/accounts/a1/trash", {
    ...routes,
    [`GET ${A}/trash`]: (c) =>
      c.path.endsWith("offset=2")
        ? { body: { items: [entry("m3")], next_offset: null, trash_retention: "30d" } }
        : { body: { items: restored ? [entry("m2")] : [entry("m1", "Проекты"), entry("m2")], next_offset: 2, trash_retention: "30d" } },
    [`POST ${A}/trash/m1/restore`]: () => {
      restored = true;
      return { body: { message: "m1", uid: 7, modseq: 42 } };
    },
  });
  expect(await screen.findByText("m1")).toBeTruthy();
  expect(await screen.findByText("Корзина: alice@example.org")).toBeTruthy();
  expect(screen.getByText("30d")).toBeTruthy();
  expect(screen.getByText("скрыть до конца срока корзины (hide)")).toBeTruthy();
  expect(screen.getAllByText("удалить окончательно (erase)")).toHaveLength(2);
  expect(sent(calls, "GET", `${A}/trash?offset=0`)).toHaveLength(1);

  fireEvent.click(screen.getAllByRole("button", { name: "Восстановить" })[0]);
  expect(await screen.findByText("Восстановить письмо в исходную папку?")).toBeTruthy();
  const buttons = screen.getAllByRole("button", { name: "Восстановить" });
  fireEvent.click(buttons[buttons.length - 1]);
  await waitFor(() => expect(sent(calls, "POST", `${A}/trash/m1/restore`)).toHaveLength(1));
  expect(await screen.findByText("Письмо восстановлено")).toBeTruthy();
  await waitFor(() => expect(screen.queryByText("m1")).toBeNull());

  fireEvent.click(screen.getByRole("button", { name: "Дальше" }));
  expect(await screen.findByText("m3")).toBeTruthy();
  expect((screen.getByRole("button", { name: "Дальше" }) as HTMLButtonElement).disabled).toBe(true);
  fireEvent.click(screen.getByRole("button", { name: "Назад" }));
  expect(await screen.findByText("m2")).toBeTruthy();
});

test("скрытые письма: список и восстановление — trash/hidden", async () => {
  const calls = open("/accounts/a1/trash", {
    ...routes,
    [`GET ${A}/trash`]: { body: { items: [], next_offset: null } },
    [`GET ${A}/trash/hidden`]: { body: { items: [entry("h1")], next_offset: null } },
    [`POST ${A}/trash/hidden/h1/restore`]: { body: { message: "h1", uid: 8, modseq: 43 } },
  });
  fireEvent.click(await screen.findByRole("tab", { name: "Скрытые письма" }));
  expect(await screen.findByText("h1")).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "Восстановить" }));
  await screen.findByText("Восстановить письмо в исходную папку?");
  const buttons = screen.getAllByRole("button", { name: "Восстановить" });
  fireEvent.click(buttons[buttons.length - 1]);
  await waitFor(() => expect(sent(calls, "POST", `${A}/trash/hidden/h1/restore`)).toHaveLength(1));
});

test("корзина: тема и отправитель — по переключателю, запрос с include=subject", async () => {
  const calls = open("/accounts/a1/trash", {
    ...routes,
    [`GET ${A}/trash`]: (c) => ({
      body: {
        items: [c.path.includes("include=subject") ? { ...entry("m1"), subject: "Счёт", from: "Боб <bob@example.org>" } : entry("m1")],
        next_offset: null,
      },
    }),
  });
  expect(await screen.findByText("m1")).toBeTruthy();
  fireEvent.click(await screen.findByRole("switch", { name: "Показать темы писем" }));
  expect(await screen.findByText("Счёт")).toBeTruthy();
  expect(screen.getByText("Боб <bob@example.org>")).toBeTruthy();
  expect(sent(calls, "GET", `${A}/trash?offset=0&include=subject`)).toHaveLength(1);
});
