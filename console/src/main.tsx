import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router";

import { loadConfig } from "./api/client";
import { App } from "./App";

const root = createRoot(document.getElementById("root") as HTMLElement);

// Адрес Admin API (`config.json`) читается до входа; ошибка конфигурации — текстом вместо консоли.
loadConfig().then(
  () =>
    root.render(
      <StrictMode>
        <BrowserRouter basename={import.meta.env.BASE_URL}>
          <App />
        </BrowserRouter>
      </StrictMode>,
    ),
  (e: Error) => root.render(<p role="alert">{e.message}</p>),
);
