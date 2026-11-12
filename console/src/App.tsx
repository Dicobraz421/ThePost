import "@refinedev/antd/dist/reset.css";
import "dayjs/locale/ru";

import { ErrorComponent, ThemedLayout, ThemedTitle, useNotificationProvider } from "@refinedev/antd";
import { Authenticated, Refine } from "@refinedev/core";
import routerProvider, { CatchAllNavigate, DocumentTitleHandler, NavigateToResource } from "@refinedev/react-router";
import { App as AntdApp, ConfigProvider } from "antd";
import ruRU from "antd/locale/ru_RU";
import dayjs from "dayjs";
import { Outlet, Route, Routes } from "react-router";

import { Header } from "./components/header";
import { i18nProvider } from "./i18n";
import { AccountCreate, AccountList, AccountShow } from "./pages/accounts";
import { DomainCreate, DomainList, DomainShow } from "./pages/domains";
import { GroupCreate, GroupEdit, GroupList } from "./pages/groups";
import { LoginPage } from "./pages/login";
import { ModerationPage } from "./pages/moderation";
import { OrganizationPage } from "./pages/organization";
import { AccountTrash } from "./pages/trash";
import { authProvider } from "./providers/auth";
import { dataProvider } from "./providers/data";

dayjs.locale("ru");

const Title = (props: { collapsed: boolean }) => <ThemedTitle {...props} text="The Post" />;

export function App() {
  return (
    <ConfigProvider locale={ruRU}>
      <AntdApp>
        <Refine
          routerProvider={routerProvider}
          dataProvider={dataProvider}
          authProvider={authProvider}
          i18nProvider={i18nProvider}
          notificationProvider={useNotificationProvider}
          resources={[
            { name: "organization", list: "/organization", meta: { label: "Организация" } },
            {
              name: "domains",
              list: "/domains",
              create: "/domains/create",
              show: "/domains/:id",
              meta: { label: "Домены" },
            },
            {
              name: "accounts",
              list: "/accounts",
              create: "/accounts/create",
              show: "/accounts/:id",
              meta: { label: "Учётные записи" },
            },
            {
              name: "groups",
              list: "/groups",
              create: "/groups/create",
              edit: "/groups/:id",
              meta: { label: "Группы рассылки" },
            },
            { name: "moderation", list: "/moderation", meta: { label: "Модерация" } },
          ]}
          // Без внешних обращений (SRS 5.2): телеметрия Refine выключена.
          options={{ disableTelemetry: true, syncWithLocation: true, warnWhenUnsavedChanges: true }}
        >
          <Routes>
            <Route
              element={
                <Authenticated key="app" fallback={<CatchAllNavigate to="/login" />}>
                  <ThemedLayout Title={Title} Header={Header}>
                    <Outlet />
                  </ThemedLayout>
                </Authenticated>
              }
            >
              <Route index element={<NavigateToResource resource="organization" />} />
              <Route path="/organization" element={<OrganizationPage />} />
              <Route path="/domains">
                <Route index element={<DomainList />} />
                <Route path="create" element={<DomainCreate />} />
                <Route path=":id" element={<DomainShow />} />
              </Route>
              <Route path="/accounts">
                <Route index element={<AccountList />} />
                <Route path="create" element={<AccountCreate />} />
                <Route path=":id" element={<AccountShow />} />
                <Route path=":id/trash" element={<AccountTrash />} />
              </Route>
              <Route path="/groups">
                <Route index element={<GroupList />} />
                <Route path="create" element={<GroupCreate />} />
                <Route path=":id" element={<GroupEdit />} />
              </Route>
              <Route path="/moderation" element={<ModerationPage />} />
              <Route path="*" element={<ErrorComponent />} />
            </Route>
            <Route
              path="/login"
              element={
                <Authenticated key="login" fallback={<LoginPage />}>
                  <NavigateToResource resource="organization" />
                </Authenticated>
              }
            />
          </Routes>
          <DocumentTitleHandler />
        </Refine>
      </AntdApp>
    </ConfigProvider>
  );
}
