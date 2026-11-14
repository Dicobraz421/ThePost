import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, expect, test } from "vitest";

import { BASE, open, sent } from "../test-app";

const AT = "2026-11-14T10:00:00Z";
const entry = (id: string, from: string, more: object = {}) => ({
  id,
  status: "ready",
  queued_at: AT,
  next_attempt: AT,
  from,
  size: 2048,
  rcpts: [{ address: "x@remote.example", status: "pending", next_attempt: AT, reply: null, attempts: 0, last_error: null }],
  ...more,
});

afterEach(() => {
  cleanup();
  sessionStorage.clear();
  localStorage.clear();
});

test("очередь: фильтры — параметрами запроса, вторая страница — по Link", async () => {
  const calls = open("/queue", {
    [`GET ${BASE}/queue`]: (c) => {
      const q = new URL(c.path, "http://x").searchParams;
      if (q.get("after") === "q1") return { body: [entry("q2", "bob@example.org")] };
      if (q.get("status") === "retry") return { body: [entry("q3", "carol@example.org", { status: "retry" })] };
      return {
        body: [entry("q1", "alice@example.org")],
        headers: { Link: `<${BASE}/queue?limit=1&after=q1>; rel="next"` },
      };
    },
  });
  expect(await screen.findByText("alice@example.org")).toBeTruthy();
  expect(screen.getByText("x@remote.example — ждёт")).toBeTruthy();
  expect(screen.getByText("Ждёт попытки")).toBeTruthy();
  expect(screen.getByText("2.0 КБ")).toBeTruthy();

  fireEvent.click(screen.getByRole("button", { name: "Показать ещё" }));
  expect(await screen.findByText("bob@example.org")).toBeTruthy();
  expect(sent(calls, "GET", `${BASE}/queue?after=q1`)).toHaveLength(1);
  await waitFor(() => expect(screen.queryByRole("button", { name: "Показать ещё" })).toBeNull());

  fireEvent.mouseDown(screen.getByRole("combobox", { name: "Состояние" }));
  fireEvent.click(await screen.findByTitle("Повтор"));
  fireEvent.change(screen.getByLabelText("Адрес"), { target: { value: " x@remote.example " } });
  fireEvent.click(screen.getByRole("button", { name: "Найти" }));
  await waitFor(() => expect(sent(calls, "GET", `${BASE}/queue?status=retry&address=x%40remote.example`)).toHaveLength(1));
  expect(await screen.findByText("carol@example.org")).toBeTruthy();
});

test("очередь: «Повторить» с подтверждением вызывает retry и обновляет строку; запись под лизой — без кнопки", async () => {
  let queue = [
    entry("q1", "alice@example.org", {
      status: "retry",
      rcpts: [{ address: "x@remote.example", status: "pending", next_attempt: AT, reply: null, attempts: 2, last_error: "451 try later" }],
    }),
    entry("q2", "bob@example.org", { status: "active" }),
  ];
  const calls = open("/queue", {
    [`GET ${BASE}/queue`]: () => ({ body: queue }),
    [`POST ${BASE}/queue/q1/retry`]: () => {
      queue = [{ ...queue[0], status: "ready" }, queue[1]];
      return { status: 202 };
    },
  });
  expect(await screen.findByText("x@remote.example — ждёт, попыток: 2")).toBeTruthy();
  expect(screen.getByText("(451 try later)", { exact: false })).toBeTruthy();
  const [first, second] = (await screen.findAllByRole("button", { name: "Повторить" })) as HTMLButtonElement[];
  expect(second.disabled).toBe(true);

  fireEvent.click(first);
  const confirm = await screen.findByText("Повторить доставку сейчас?");
  fireEvent.click(within(confirm.closest(".ant-popover") as HTMLElement).getByRole("button", { name: "Повторить" }));
  await waitFor(() => expect(sent(calls, "POST", `${BASE}/queue/q1/retry`)).toHaveLength(1));
  expect(await screen.findByText("Повтор доставки запрошен")).toBeTruthy();
  await waitFor(() => expect(screen.queryByText("Повтор")).toBeNull());
  expect(screen.getAllByText("Ждёт попытки")).toHaveLength(1);
});

test("очередь: оператору — только просмотр", async () => {
  open("/queue", { [`GET ${BASE}/queue`]: { body: [entry("q1", "alice@example.org")] } }, ["operator"]);
  expect(await screen.findByText("alice@example.org")).toBeTruthy();
  await waitFor(() => expect(screen.queryByRole("button", { name: "Повторить" })).toBeNull());
});
