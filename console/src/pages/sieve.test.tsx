import { cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, test } from "vitest";

import { BASE, buttons, open, sent } from "../test-app";

const A = `${BASE}/accounts/a1`;
const account = { [`GET ${A}`]: { body: { id: "a1", login: "alice@example.org", policy: {}, effective: {} } } };
const text = () => screen.getByLabelText("Текст скрипта") as HTMLTextAreaElement;
const last = async (name: string) => (await buttons(name)).at(-1) as HTMLButtonElement;

afterEach(() => {
  cleanup();
  sessionStorage.clear();
  localStorage.clear();
});

test("правила учётной записи: редактор, ошибки проверки с позициями, активация, удаление", async () => {
  let valid = false;
  const calls = open("/accounts/a1/sieve", {
    ...account,
    [`GET ${A}/sieve/scripts`]: { body: [{ name: "main", size: 12, active: true }, { name: "draft", size: 5, active: false }] },
    [`GET ${A}/sieve/scripts/draft`]: { body: { name: "draft", size: 5, active: false, text: "keep;" } },
    [`PUT ${A}/sieve/scripts/draft`]: () =>
      valid
        ? { status: 204 }
        : { status: 422, body: { status: 422, title: "Unprocessable Entity", errors: [{ line: 2, column: 1, message: "неизвестная команда" }] } },
    [`POST ${A}/sieve/scripts/draft/activate`]: { status: 204 },
    [`DELETE ${A}/sieve/scripts/draft`]: { status: 204 },
    [`POST ${A}/sieve/deactivate`]: { status: 204 },
  });
  expect(await screen.findByText("Правила и автоответ: alice@example.org")).toBeTruthy();
  expect(await screen.findByText("активен")).toBeTruthy();
  // Активный скрипт не удаляется и не активируется повторно: кнопки — только у второго.
  expect(screen.getAllByRole("button", { name: "Активировать" })).toHaveLength(1);

  fireEvent.click(screen.getAllByRole("button", { name: "Открыть" })[1]);
  await waitFor(() => expect(text().value).toBe("keep;"));
  fireEvent.change(text(), { target: { value: "keep;\nfoo;" } });
  fireEvent.click(screen.getByRole("button", { name: "Сохранить скрипт" }));
  expect(await screen.findByText("Скрипт не сохранён: ошибки в тексте")).toBeTruthy();
  expect(screen.getByText(/неизвестная команда/)).toBeTruthy();
  expect(sent(calls, "PUT", `${A}/sieve/scripts/draft`)[0].body).toEqual({ text: "keep;\nfoo;" });
  // Нажатие на позицию выделяет строку с ошибкой.
  fireEvent.click(screen.getByRole("button", { name: "Строка 2, столбец 1" }));
  expect([text().selectionStart, text().selectionEnd]).toEqual([6, 10]);

  valid = true;
  fireEvent.click(screen.getByRole("button", { name: "Сохранить скрипт" }));
  expect(await screen.findByText("Скрипт draft сохранён")).toBeTruthy();
  await waitFor(() => expect(screen.queryByText("Скрипт не сохранён: ошибки в тексте")).toBeNull());

  fireEvent.click(screen.getByRole("button", { name: "Активировать" }));
  await waitFor(() => expect(sent(calls, "POST", `${A}/sieve/scripts/draft/activate`)).toHaveLength(1));
  fireEvent.click(screen.getByRole("button", { name: "Удалить" }));
  await screen.findByText("Удалить скрипт draft?");
  fireEvent.click(await last("Удалить"));
  await waitFor(() => expect(sent(calls, "DELETE", `${A}/sieve/scripts/draft`)).toHaveLength(1));
  fireEvent.click(screen.getByRole("button", { name: "Отключить правила" }));
  fireEvent.click(await screen.findByRole("button", { name: "Отключить" }));
  await waitFor(() => expect(sent(calls, "POST", `${A}/sieve/deactivate`)).toHaveLength(1));
});

test("новый скрипт — PUT по введённому имени", async () => {
  const calls = open("/accounts/a1/sieve", {
    ...account,
    [`GET ${A}/sieve/scripts`]: { body: [] },
    [`PUT ${A}/sieve/scripts/vacation rules`]: { status: 204 },
  });
  fireEvent.click(await screen.findByRole("button", { name: "Новый скрипт" }));
  fireEvent.change(await screen.findByLabelText("Имя скрипта"), { target: { value: "vacation rules" } });
  fireEvent.change(text(), { target: { value: "discard;" } });
  fireEvent.click(screen.getByRole("button", { name: "Сохранить скрипт" }));
  await waitFor(() => expect(sent(calls, "PUT", `${A}/sieve/scripts/vacation%20rules`)[0]?.body).toEqual({ text: "discard;" }));
});

test("автоответ за пользователя — PUT vacation", async () => {
  const vacation = { enabled: false, from: null, to: "2027-01-11T00:00:00Z", subject: null, text_body: "В отпуске.", html_body: null };
  const calls = open("/accounts/a1/sieve", {
    ...account,
    [`GET ${A}/sieve/scripts`]: { body: [] },
    [`GET ${A}/vacation`]: { body: vacation },
    [`PUT ${A}/vacation`]: (c) => ({ body: c.body }),
  });
  fireEvent.click(await screen.findByRole("tab", { name: "Автоответ" }));
  fireEvent.click(await screen.findByLabelText("Автоответ включён"));
  fireEvent.change(screen.getByLabelText("Тема"), { target: { value: " Отпуск " } });
  fireEvent.click(screen.getByRole("button", { name: "Сохранить автоответ" }));
  await waitFor(() =>
    expect(sent(calls, "PUT", `${A}/vacation`)[0]?.body).toEqual({
      ...vacation,
      enabled: true,
      to: "2027-01-11T00:00:00.000Z",
      subject: "Отпуск",
    }),
  );
  expect(await screen.findByText("Автоответ сохранён")).toBeTruthy();
});

test("скрипт организации: нет скрипта — пустой редактор; запись и удаление", async () => {
  let stored: string | null = null;
  const calls = open("/sieve", {
    [`GET ${BASE}/sieve`]: () => (stored === null ? { status: 404, body: { status: 404, title: "Not Found" } } : { body: { text: stored } }),
    [`PUT ${BASE}/sieve`]: (c) => {
      stored = (c.body as { text: string }).text;
      return { status: 204 };
    },
    [`DELETE ${BASE}/sieve`]: () => {
      stored = null;
      return { status: 204 };
    },
  });
  expect(await screen.findByText(/Сейчас скрипта нет/)).toBeTruthy();
  expect(text().value).toBe("");
  expect(screen.queryByRole("button", { name: "Удалить скрипт" })).toBeNull();
  fireEvent.change(text(), { target: { value: "keep;" } });
  fireEvent.click(screen.getByRole("button", { name: "Сохранить скрипт" }));
  expect(await screen.findByText("Скрипт организации сохранён")).toBeTruthy();
  expect(sent(calls, "PUT", `${BASE}/sieve`)[0].body).toEqual({ text: "keep;" });

  fireEvent.click(await screen.findByRole("button", { name: "Удалить скрипт" }));
  await screen.findByText("Удалить скрипт организации?");
  fireEvent.click(await last("Удалить"));
  await waitFor(() => expect(sent(calls, "DELETE", `${BASE}/sieve`)).toHaveLength(1));
  await waitFor(() => expect(text().value).toBe(""));
});
