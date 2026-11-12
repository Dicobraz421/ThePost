import "@refinedev/antd/dist/reset.css";
import "dayjs/locale/ru";

import { ErrorComponent, ThemedLayout, ThemedTitle, useNotificationProvider } from "@refinedev/antd";
import { Authenticated, Refine } from "@refinedev/core";
import routerProvider, { CatchAllNavigate, DocumentTitleHandler } from "@refinedev/react-router";
import { App as AntdApp, ConfigProvider } from "antd";
import ruRU from "antd/locale/ru_RU";
import dayjs from "dayjs";
import { Outlet, Route, Routes } from "react-router";

import { i18nProvider } from "./i18n";
import { LoginPage } from "./pages/login";
import { authProvider } from "./providers/auth";

dayjs.locale("ru");

const Title = (props: { collapsed: boolean }) => <ThemedTitle {...props} text="The Post" />;

export function App() {
  return (
    <ConfigProvider locale={ruRU}>
      <AntdApp>
        <Refine
          routerProvider={routerProvider}
          authProvider={authProvider}
          i18nProvider={i18nProvider}
          notificationProvider={useNotificationProvider}
          // Без внешних обращений (SRS 5.2): телеметрия Refine выключена.
          options={{ disableTelemetry: true, syncWithLocation: true, warnWhenUnsavedChanges: true }}
        >
          <Routes>
            <Route
              element={
                <Authenticated key="app" fallback={<CatchAllNavigate to="/login" />}>
                  <ThemedLayout Title={Title}>
                    <Outlet />
                  </ThemedLayout>
                </Authenticated>
              }
            >
              <Route path="*" element={<ErrorComponent />} />
            </Route>
            <Route path="/login" element={<LoginPage />} />
          </Routes>
          <DocumentTitleHandler />
        </Refine>
      </AntdApp>
    </ConfigProvider>
  );
}
