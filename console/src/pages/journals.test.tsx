import { cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, test } from "vitest";

import { allowed } from "../providers/access";
import { BASE, open, sent } from "../test-app";

const event = (kind: string, decision: string, o: object = {}) => ({
  at: "2026-11-10T10:00:00Z",
  node: "n1",
  kind,
  sender: "alice@example.org",
  recipients: ["bob@example.net"],
  decision,
  message_id: "m1@example.org",
  queue_id: null,
  reason: null,
  ...o,
});
const record = (seq: number, actor_login: string, method: string, route: string, status = 200) => ({
  seq,
  at: "2026-11-10T10:00:00Z",
  actor_tenant: "t",
  actor: "a",
  actor_login,
  roles: ["organization_admin"],
  method,
  route,
  path: route.replace("{org}", "t"),
  status,
  tenant: "t",
});

afterEach(() => {
  cleanup();
  sessionStorage.clear();
  localStorage.clear();
});

test("журнал доставки: поиск по отправителю и Message-ID, страницы по next, карточка письма", async () => {
  const calls = open("/delivery", {
    [`GET ${BASE}/delivery`]: (c) =>
      c.path.includes("message_id=m1")
        ? { body: { events: [event("receive", "accept"), event("relay", "defer", { reason: "451 позже", queue_id: "q7" })], next: null } }
        : c.path.includes("after=c2")
          ? { body: { events: [event("dsn", "accept", { sender: "", message_id: null })], next: null } }
          : { body: { events: [event("receive", "accept")], next: "c2" } },
  });
  expect(await screen.findByText("alice@example.org")).toBeTruthy();
  expect(screen.getByText("приём")).toBeTruthy();

  fireEvent.change(screen.getByLabelText("Отправитель"), { target: { value: "alice@example.org" } });
  fireEvent.change(screen.getByLabelText("Message-ID"), { target: { value: "<x@example.org>" } });
  fireEvent.click(screen.getByRole("button", { name: "Найти" }));
  const query = `${BASE}/delivery?sender=alice%40example.org&message_id=x%40example.org`;
  await waitFor(() => expect(sent(calls, "GET", query)).toHaveLength(1));

  fireEvent.click(await screen.findByRole("button", { name: "Дальше" }));
  expect(await screen.findByText("<> (уведомление о недоставке)")).toBeTruthy();
  expect(sent(calls, "GET", `${query}&after=c2`)).toHaveLength(1);
  fireEvent.click(screen.getByRole("button", { name: "Назад" }));

  fireEvent.click(await screen.findByRole("button", { name: "m1@example.org" }));
  expect(await screen.findByText("Причина: 451 позже")).toBeTruthy();
  expect(screen.getByText(/запись очереди q7/)).toBeTruthy();
  expect(sent(calls, "GET", `${BASE}/delivery?message_id=m1%40example.org`)).toHaveLength(1);
});

test("журнал аудита: записи, отбор по администратору и действию на странице, страницы по next", async () => {
  const calls = open("/audit", {
    [`GET ${BASE}/audit`]: (c) =>
      c.path.includes("after=2")
        ? { body: { entries: [record(3, "carol@example.org", "GET", "/api/v1/orgs/{org}/audit")], next: null } }
        : {
            body: {
              entries: [
                record(1, "alice@example.org", "POST", "/api/v1/orgs/{org}/transport/domains", 201),
                record(2, "bob@example.org", "DELETE", "/api/v1/orgs/{org}/groups/{group}", 403),
              ],
              next: 2,
            },
          },
  });
  expect(await screen.findByText("alice@example.org")).toBeTruthy();
  expect(screen.getByText("DELETE /api/v1/orgs/{org}/groups/{group}")).toBeTruthy();
  expect(screen.getAllByText("администратор организации")).toHaveLength(2);

  fireEvent.change(screen.getByLabelText("Администратор"), { target: { value: "BOB" } });
  await waitFor(() => expect(screen.queryByText("alice@example.org")).toBeNull());
  expect(screen.getByText("bob@example.org")).toBeTruthy();
  fireEvent.change(screen.getByLabelText("Действие"), { target: { value: "post" } });
  await waitFor(() => expect(screen.queryByText("bob@example.org")).toBeNull());
  expect(screen.getByText(/показано 0 из 2/)).toBeTruthy();
  fireEvent.change(screen.getByLabelText("Администратор"), { target: { value: "" } });
  fireEvent.change(screen.getByLabelText("Действие"), { target: { value: "" } });

  fireEvent.click(screen.getByRole("button", { name: "Дальше" }));
  expect(await screen.findByText("carol@example.org")).toBeTruthy();
  expect(sent(calls, "GET", `${BASE}/audit?after=2`)).toHaveLength(1);
});

test("аудитор: консоль открывается на журнале аудита, других разделов не запрашивает", async () => {
  const calls = open("/", { [`GET ${BASE}/audit`]: { body: { entries: [record(1, "alice@example.org", "GET", "/x")], next: null } } }, ["auditor"]);
  expect(await screen.findByText("alice@example.org")).toBeTruthy();
  expect(calls.map((c) => c.path).filter((p) => p !== "/api/v1/whoami" && !p.startsWith(`${BASE}/audit`))).toEqual([]);
});

test("роли: аудитор — только журнал аудита, оператор — чтение без журнала аудита", () => {
  expect(allowed(["auditor"], "audit", "list")).toBe(true);
  expect(allowed(["auditor"], "accounts", "list")).toBe(false);
  expect(allowed(["operator"], "accounts", "list")).toBe(true);
  expect(allowed(["operator"], "accounts", "create")).toBe(false);
  expect(allowed(["operator"], "audit", "list")).toBe(false);
  expect(allowed(["organization_admin"], "audit", "list")).toBe(true);
  expect(allowed(["organization_admin"], "groups", "delete")).toBe(true);
  expect(allowed([], "organization", "list")).toBe(false);
});
