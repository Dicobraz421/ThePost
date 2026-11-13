import { cleanup, fireEvent, screen } from "@testing-library/react";
import { afterEach, expect, test } from "vitest";

import { ORG } from "../test-api";
import { BASE, open, sent } from "../test-app";

const empty = { quota_bytes: null };

afterEach(() => {
  cleanup();
  sessionStorage.clear();
  localStorage.clear();
});

test("оператор по прямому адресу экрана администратора видит «Нет доступа» и уходит на начальный экран", async () => {
  const calls = open("/accounts/create", { [`GET ${BASE}`]: { body: { id: ORG, default: true, policy: empty, effective: empty } } }, [
    "operator",
  ]);
  expect(await screen.findByText("Нет доступа")).toBeTruthy();
  expect(screen.queryByLabelText("Имя входа")).toBeNull();
  fireEvent.click(screen.getByRole("button", { name: "На начальный экран" }));
  expect(await screen.findByText("Политика организации")).toBeTruthy();
  expect(sent(calls, "POST", `${BASE}/accounts`)).toHaveLength(0);
});

test("журнал аудита оператору недоступен, аудитору недоступны остальные разделы", async () => {
  open("/audit", {}, ["operator"]);
  expect(await screen.findByText("Нет доступа")).toBeTruthy();
  cleanup();
  const calls = open("/accounts", {}, ["auditor"]);
  expect(await screen.findByText("Нет доступа")).toBeTruthy();
  expect(calls.some((c) => c.path.startsWith(`${BASE}/accounts`))).toBe(false);
});
