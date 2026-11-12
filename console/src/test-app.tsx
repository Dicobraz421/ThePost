// Открытие экрана консоли в тестах: вошедший администратор, выбранная организация, заглушка Admin API.
import { render } from "@testing-library/react";
import { MemoryRouter } from "react-router";

import { saveSession, setOrg } from "./api/client";
import { App } from "./App";
import { type Call, ORG, session, stubApi, whoami } from "./test-api";

export const BASE = `/api/v1/orgs/${ORG}`;

export function open(path: string, routes: Parameters<typeof stubApi>[0]) {
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

/** Запросы с этим методом и путём (путь — со строкой запроса, если она была). */
export const sent = (calls: Call[], method: string, path: string) => calls.filter((c) => c.method === method && c.path === path);
