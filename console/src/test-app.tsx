// Открытие экрана консоли в тестах: вошедший администратор, выбранная организация, заглушка Admin API.
import { render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router";

import { saveSession, setOrg } from "./api/client";
import { App } from "./App";
import { type Call, ORG, session, stubApi, whoami } from "./test-api";

export const BASE = `/api/v1/orgs/${ORG}`;

/** `roles` — роли вошедшего (по умолчанию — администратор инсталляции). */
export function open(path: string, routes: Parameters<typeof stubApi>[0], roles: string[] = whoami.roles) {
  const s = session();
  saveSession({ token: s.session, expires_at: s.expires_at });
  setOrg(ORG);
  const calls = stubApi({ "GET /api/v1/whoami": { body: { ...whoami, roles } }, ...routes });
  render(
    <MemoryRouter initialEntries={[path]}>
      <App />
    </MemoryRouter>,
  );
  return calls;
}

/** Запросы с этим методом и путём (путь — со строкой запроса, если она была). */
export const sent = (calls: Call[], method: string, path: string) => calls.filter((c) => c.method === method && c.path === path);

/** Кнопки с проверкой роли (Refine спрашивает роли из `whoami`) появляются и включаются не сразу. */
export const buttons = (name: string | RegExp) =>
  waitFor(() => {
    const found = screen.getAllByRole("button", { name }) as HTMLButtonElement[];
    if (found.some((b) => b.disabled)) throw new Error(`кнопка ${name} выключена`);
    return found;
  });
