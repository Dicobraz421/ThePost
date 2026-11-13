import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, expect, test } from "vitest";

import { DEFAULT_BOOKING, fromBooking, toBooking } from "../components/booking-policy-form";
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
const resource = (id: string, login: string, name: string, booking: object | null = DEFAULT_BOOKING) => ({
  id,
  login,
  domain: "d1",
  display_name: name,
  enabled: true,
  external_id: null,
  aliases: [],
  hidden_from_gal: false,
  quota: { used_bytes: 0, messages: 0, limit_bytes: null },
  policy: empty,
  effective: empty,
  booking_policy: booking,
});

afterEach(() => {
  cleanup();
  sessionStorage.clear();
  localStorage.clear();
});

test("правила бронирования ↔ форма: по умолчанию, ограниченный круг, рабочее время", () => {
  expect(toBooking(fromBooking())).toEqual(DEFAULT_BOOKING);
  const p = {
    auto_accept: false,
    allow_conflicts: true,
    max_duration: null,
    horizon: "30d",
    bookers: { type: "only" as const, accounts: ["a1"], groups: ["g1"] },
    working_hours: { timezone: "Europe/Moscow", days: ["monday" as const, "friday" as const], from: "09:00", to: "18:30" },
  };
  expect(toBooking(fromBooking(p))).toEqual(p);
  expect(toBooking({ ...fromBooking(p), bookers: "internal", hours: false, horizon: " " })).toMatchObject({
    bookers: { type: "internal" },
    working_hours: null,
    horizon: null,
  });
});

test("ресурсы: список с правилами, удаление — DELETE", async () => {
  let list = [
    resource("r1", "room@example.org", "Переговорная 5.12"),
    resource("r2", "beamer@example.org", "Проектор", { ...DEFAULT_BOOKING, auto_accept: false, working_hours: { timezone: "UTC", days: ["monday"], from: "09:00", to: "18:00" } }),
  ];
  const calls = open("/resources", {
    [`GET ${BASE}/resources`]: () => ({ body: list }),
    [`DELETE ${BASE}/resources/r1`]: () => {
      list = list.slice(1);
      return { status: 204 };
    },
  });
  expect(await screen.findByText("Переговорная 5.12")).toBeTruthy();
  expect(screen.getByText("автопринятие, до 1d")).toBeTruthy();
  expect(screen.getByText("по решению представителя, до 1d, 09:00–18:00")).toBeTruthy();
  fireEvent.click((await buttons("Удалить"))[0]);
  const confirm = await screen.findByText("Удалить ресурс Переговорная 5.12?");
  fireEvent.click(within(confirm.closest(".ant-popover") as HTMLElement).getByRole("button", { name: "Удалить" }));
  await waitFor(() => expect(sent(calls, "DELETE", `${BASE}/resources/r1`)).toHaveLength(1));
  await waitFor(() => expect(screen.queryByText("Переговорная 5.12")).toBeNull());
});

test("ресурсы: оператор видит список без удаления", async () => {
  open("/resources", { [`GET ${BASE}/resources`]: { body: [resource("r1", "room@example.org", "Переговорная")] } }, ["operator"]);
  expect(await screen.findByText("Переговорная")).toBeTruthy();
  await waitFor(() => expect(screen.queryByRole("button", { name: "Удалить" })).toBeNull());
});

test("ресурс: создание — POST с правилами бронирования", async () => {
  const calls = open("/resources/create", {
    [`POST ${BASE}/resources`]: (c) => ({ status: 201, body: { ...resource("r9", "room@example.org", "Комната"), ...(c.body as object) } }),
  });
  const [save] = await buttons(/Сохранить/);
  fireEvent.change(await screen.findByLabelText("Адрес"), { target: { value: "room@example.org" } });
  fireEvent.change(screen.getByLabelText("Название"), { target: { value: " Комната " } });
  fireEvent.change(screen.getByLabelText("Наибольшая длительность встречи"), { target: { value: "4h" } });
  fireEvent.click(screen.getByRole("switch", { name: "Разрешить пересечения" }));
  fireEvent.click(screen.getByRole("switch", { name: "Только в рабочее время" }));
  fireEvent.change(await screen.findByLabelText("Конец дня"), { target: { value: "17:00" } });
  fireEvent.click(save);
  await waitFor(() =>
    expect(sent(calls, "POST", `${BASE}/resources`)[0]?.body).toEqual({
      login: "room@example.org",
      display_name: "Комната",
      booking_policy: {
        ...DEFAULT_BOOKING,
        allow_conflicts: true,
        max_duration: "4h",
        working_hours: { timezone: "Europe/Moscow", days: ["monday", "tuesday", "wednesday", "thursday", "friday"], from: "09:00", to: "17:00" },
      },
    }),
  );
});

test("ресурс: правила — PUT booking-policy, представители ресурса", async () => {
  const calls = open("/resources/r1", {
    [`GET ${BASE}/resources/r1`]: { body: resource("r1", "room@example.org", "Переговорная 5.12") },
    [`GET ${BASE}/accounts/r1/delegates`]: {
      body: [{ delegate: "a2", login: "boss@example.org", display_name: "Босс", level: "write", schedule_on_behalf: true }],
    },
    [`PUT ${BASE}/resources/r1/booking-policy`]: (c) => ({ body: c.body }),
  });
  expect(await screen.findByText("boss@example.org")).toBeTruthy();
  fireEvent.click(await screen.findByRole("switch", { name: "Принимать приглашения автоматически" }));
  fireEvent.change(screen.getByLabelText("Бронировать вперёд не более"), { target: { value: "" } });
  fireEvent.click(screen.getByRole("button", { name: "Сохранить правила" }));
  await waitFor(() =>
    expect(sent(calls, "PUT", `${BASE}/resources/r1/booking-policy`)[0]?.body).toEqual({ ...DEFAULT_BOOKING, auto_accept: false, horizon: null }),
  );
  expect(await screen.findByText("Правила бронирования сохранены")).toBeTruthy();
});
