import { render, screen, within } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { expect, test } from "vitest";

import { App } from "./App";
import { stubApi } from "./test-api";

test("без входа открывается экран входа на русском", async () => {
  render(
    <MemoryRouter initialEntries={["/"]}>
      <App />
    </MemoryRouter>,
  );
  expect(await screen.findByText("Имя входа")).toBeTruthy();
  expect(screen.getByText("Пароль")).toBeTruthy();
  expect(screen.getByRole("button", { name: "Войти" })).toBeTruthy();
});

test("вход по паролю выключен — форма токена admin init", async () => {
  stubApi({ "GET /api/v1/session": { body: { oidc: null, password: false } } });
  const page = within(
    render(
      <MemoryRouter initialEntries={["/login"]}>
        <App />
      </MemoryRouter>,
    ).container,
  );
  expect(await page.findByText("Токен администратора")).toBeTruthy();
  expect(page.queryByText("Имя входа")).toBeNull();
  stubApi({});
});
