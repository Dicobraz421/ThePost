import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { afterEach, expect, test } from "vitest";

import { saveSession, setOrg } from "../api/client";
import { App } from "../App";
import { toBody } from "../components/policy-form";
import { type Call, ORG, session, stubApi, whoami } from "../test-api";
import { buttons } from "../test-app";

const BASE = `/api/v1/orgs/${ORG}`;
const empty = {
  dkim_selector: null,
  max_recipients: null,
  milter_profile: null,
  quota_bytes: null,
  retention: null,
  send_per_hour: null,
  trash_purge: null,
  trash_retention: null,
};

function open(path: string, routes: Parameters<typeof stubApi>[0]) {
  const s = session();
  saveSession({ token: s.session, expires_at: s.expires_at });
  setOrg(ORG);
  const calls = stubApi({ "GET /api/v1/whoami": { body: whoami }, ...routes });
  render(
    <MemoryRouter initialEntries={[path]}>
      <App />
    </MemoryRouter>,
  );
  return calls;
}

// Форма пересоздаётся, когда приходит политика: поле ищется заново.
const field = (label: string) => screen.getByLabelText(label) as HTMLInputElement;
const sent = (calls: Call[], method: string, path: string) => calls.find((c) => c.method === method && c.path === path);

afterEach(() => {
  cleanup();
  sessionStorage.clear();
  localStorage.clear();
});

test("организация: политика по схеме OpenAPI, сохранение — PUT policy", async () => {
  const calls = open("/", {
    [`GET ${BASE}`]: {
      body: {
        id: ORG,
        default: true,
        policy: { ...empty, quota_bytes: 100 },
        effective: { ...empty, quota_bytes: 100, trash_retention: "30d" },
      },
    },
    [`PUT ${BASE}/policy`]: (c) => ({ body: c.body }),
  });
  await waitFor(() => expect(field("Квота ящика, байт.").value).toBe("100"));
  expect(screen.getByText("Действует: 30d")).toBeTruthy();
  expect(screen.getByText("admin@example.org")).toBeTruthy();

  fireEvent.change(screen.getByLabelText("Срок хранения в корзине (`30d`, `12h`)."), { target: { value: "7d" } });
  fireEvent.click(screen.getByRole("button", { name: "Сохранить политику" }));
  await waitFor(() =>
    expect(sent(calls, "PUT", `${BASE}/policy`)?.body).toEqual({ ...empty, quota_bytes: 100, trash_retention: "7d" }),
  );
  expect(await screen.findByText("Политика организации сохранена")).toBeTruthy();
});

test("домены: список и удаление", async () => {
  const calls = open("/domains", {
    [`GET ${BASE}/domains`]: { body: [{ id: "d1", name: "example.org" }, { id: "d2", name: "example.net" }] },
    [`DELETE ${BASE}/domains/d1`]: { status: 204 },
  });
  expect(await screen.findByText("example.org")).toBeTruthy();
  expect(screen.getByText("example.net")).toBeTruthy();
  fireEvent.click((await buttons(/Удалить/))[0]);
  expect(await screen.findByText("Удалить домен example.org?")).toBeTruthy();
  const all = screen.getAllByRole("button", { name: /Удалить/ });
  fireEvent.click(all[all.length - 1]);
  await waitFor(() => expect(sent(calls, "DELETE", `${BASE}/domains/d1`)).toBeTruthy());
});

test("домены: добавление; ошибка API — в уведомлении", async () => {
  const calls = open("/domains/create", {
    [`POST ${BASE}/domains`]: { status: 409, body: { status: 409, title: "Conflict", detail: "домен уже есть" } },
  });
  fireEvent.change(await screen.findByLabelText("Имя домена"), { target: { value: "example.org" } });
  fireEvent.click((await buttons(/Сохранить/))[0]);
  await waitFor(() => expect(sent(calls, "POST", `${BASE}/domains`)?.body).toEqual({ name: "example.org" }));
  expect(await screen.findByText("домен уже есть")).toBeTruthy();
});

test("домен: политика домена — PUT domains/{domain}/policy", async () => {
  const calls = open("/domains/d1", {
    [`GET ${BASE}/domains/d1`]: {
      body: { id: "d1", name: "example.org", policy: { ...empty, dkim_selector: "s1" }, effective: empty },
    },
    [`PUT ${BASE}/domains/d1/policy`]: (c) => ({ body: c.body }),
  });
  await waitFor(() => expect(field("Селектор ключа DKIM.").value).toBe("s1"));
  fireEvent.change(field("Селектор ключа DKIM."), { target: { value: "s2" } });
  fireEvent.click(screen.getByRole("button", { name: "Сохранить политику" }));
  await waitFor(() => expect(sent(calls, "PUT", `${BASE}/domains/d1/policy`)?.body).toEqual({ ...empty, dkim_selector: "s2" }));
});

test("тело политики: пустое — null, вложенное правило без обязательного поля не задано", () => {
  const root = { $ref: "#/components/schemas/PolicyBody" };
  expect(toBody(root, { retention: "", trash_purge: { user_can_choose: true } })).toEqual(empty);
  expect(toBody(root, { quota_bytes: 0, trash_purge: { mode: "hide" } })).toEqual({
    ...empty,
    quota_bytes: 0,
    trash_purge: { mode: "hide" },
  });
});
