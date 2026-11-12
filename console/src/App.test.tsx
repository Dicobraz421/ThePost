import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { expect, test } from "vitest";

import { App } from "./App";

test("без входа открывается экран входа на русском", async () => {
  render(
    <MemoryRouter initialEntries={["/"]}>
      <App />
    </MemoryRouter>,
  );
  expect(await screen.findByText("Токен администратора")).toBeTruthy();
  expect(screen.getByRole("button", { name: "Войти" })).toBeTruthy();
});
