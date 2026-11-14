import "@refinedev/antd/dist/reset.css";
import "dayjs/locale/ru";

import { ErrorComponent, ThemedLayout, ThemedTitle, useNotificationProvider } from "@refinedev/antd";
import { Authenticated, CanAccess, Refine, usePermissions } from "@refinedev/core";
import routerProvider, { CatchAllNavigate, DocumentTitleHandler, NavigateToResource } from "@refinedev/react-router";
import { App as AntdApp, ConfigProvider } from "antd";
// ES-модуль: `antd/locale/ru_RU` (CommonJS) в сборке Vite приходит обёрткой без текстов — интерфейс остаётся английским.
import ruRU from "antd/es/locale/ru_RU";
import dayjs from "dayjs";
import { Outlet, Route, Routes } from "react-router";

import { Forbidden } from "./components/forbidden";
import { Header } from "./components/header";
import { i18nProvider } from "./i18n";
import type { Schemas } from "./api/client";
import { AccountCreate, AccountList, AccountShow } from "./pages/accounts";
import { AuditPage } from "./pages/audit";
import { ContactCreate, ContactEdit, ContactList } from "./pages/contacts";
import { DeliveryPage } from "./pages/delivery";
import { DisabledPage } from "./pages/disabled";
import { GroupCreate, GroupEdit, GroupList } from "./pages/groups";
import { LoginPage } from "./pages/login";
import { MailboxAuditPage } from "./pages/mailbox-audit";
import { ModerationPage } from "./pages/moderation";
import { OrganizationPage } from "./pages/organization";
import { QueuePage } from "./pages/queue";
import { ResourceCreate, ResourceList, ResourceShow } from "./pages/resources";
import { AccountSieve, OrgSieve } from "./pages/sieve";
import { AccountTrash } from "./pages/trash";
import { TransportPage } from "./pages/transport";
import { UnitsPage } from "./pages/units";
import { accessControlProvider, allowed } from "./providers/access";
import { authProvider } from "./providers/auth";
import { dataProvider } from "./providers/data";

dayjs.locale("ru");

const Title = (props: { collapsed: boolean }) => <ThemedTitle {...props} text="The Post" />;

/** Первый раздел по роли: аудитору доступен только журнал аудита. */
function Home() {
  const { data: roles, isLoading } = usePermissions<Schemas["AdminRole"][]>({});
  if (isLoading) return null;
  return <NavigateToResource resource={allowed(roles ?? ["installation_admin"], "organization", "list") ? "organization" : "audit"} />;
}

export function App() {
  return (
    <ConfigProvider locale={ruRU}>
      <AntdApp>
        <Refine
          routerProvider={routerProvider}
          dataProvider={dataProvider}
          authProvider={authProvider}
          accessControlProvider={accessControlProvider}
          i18nProvider={i18nProvider}
          notificationProvider={useNotificationProvider}
          resources={[
            { name: "organization", list: "/organization", meta: { label: "Организация" } },
            {
              name: "accounts",
              list: "/accounts",
              create: "/accounts/create",
              show: "/accounts/:id",
              meta: { label: "Учётные записи" },
            },
            {
              name: "resources",
              list: "/resources",
              create: "/resources/create",
              show: "/resources/:id",
              meta: { label: "Ресурсы" },
            },
            { name: "disabled-accounts", list: "/disabled-accounts", meta: { label: "Отключённые ящики" } },
            {
              name: "groups",
              list: "/groups",
              create: "/groups/create",
              edit: "/groups/:id",
              meta: { label: "Группы рассылки" },
            },
            {
              name: "contacts",
              list: "/contacts",
              create: "/contacts/create",
              edit: "/contacts/:id",
              meta: { label: "Адресная книга" },
            },
            { name: "units", list: "/units", meta: { label: "Оргструктура" } },
            { name: "moderation", list: "/moderation", meta: { label: "Модерация" } },
            { name: "queue", list: "/queue", meta: { label: "Очередь" } },
            { name: "sieve", list: "/sieve", meta: { label: "Правила организации" } },
            { name: "transport", list: "/transport", meta: { label: "Транспорт" } },
            { name: "delivery", list: "/delivery", meta: { label: "Журнал доставки" } },
            { name: "audit", list: "/audit", meta: { label: "Журнал аудита" } },
            { name: "mailbox-audit", list: "/mailbox-audit", meta: { label: "Аудит ящиков" } },
          ]}
          // Без внешних обращений (SRS 5.2): телеметрия Refine выключена.
          options={{ disableTelemetry: true, syncWithLocation: true, warnWhenUnsavedChanges: true }}
        >
          <Routes>
            <Route
              element={
                <Authenticated key="app" fallback={<CatchAllNavigate to="/login" />}>
                  <ThemedLayout Title={Title} Header={Header}>
                    {/* Раздел или действие по маршруту, недоступные роли, — «Нет доступа» вместо отказов узла в таблице. */}
                    <CanAccess fallback={<Forbidden />}>
                      <Outlet />
                    </CanAccess>
                  </ThemedLayout>
                </Authenticated>
              }
            >
              <Route index element={<Home />} />
              <Route path="/organization" element={<OrganizationPage />} />
              <Route path="/accounts">
                <Route index element={<AccountList />} />
                <Route path="create" element={<AccountCreate />} />
                <Route path=":id" element={<AccountShow />} />
                <Route path=":id/trash" element={<AccountTrash />} />
                <Route path=":id/sieve" element={<AccountSieve />} />
              </Route>
              <Route path="/resources">
                <Route index element={<ResourceList />} />
                <Route path="create" element={<ResourceCreate />} />
                <Route path=":id" element={<ResourceShow />} />
              </Route>
              <Route path="/groups">
                <Route index element={<GroupList />} />
                <Route path="create" element={<GroupCreate />} />
                <Route path=":id" element={<GroupEdit />} />
              </Route>
              <Route path="/disabled-accounts" element={<DisabledPage />} />
              <Route path="/contacts">
                <Route index element={<ContactList />} />
                <Route path="create" element={<ContactCreate />} />
                <Route path=":id" element={<ContactEdit />} />
              </Route>
              <Route path="/units" element={<UnitsPage />} />
              <Route path="/moderation" element={<ModerationPage />} />
              <Route path="/queue" element={<QueuePage />} />
              <Route path="/sieve" element={<OrgSieve />} />
              <Route path="/transport" element={<TransportPage />} />
              <Route path="/delivery" element={<DeliveryPage />} />
              <Route path="/audit" element={<AuditPage />} />
              <Route path="/mailbox-audit" element={<MailboxAuditPage />} />
              <Route path="*" element={<ErrorComponent />} />
            </Route>
            <Route
              path="/login"
              element={
                <Authenticated key="login" fallback={<LoginPage />}>
                  <Home />
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
