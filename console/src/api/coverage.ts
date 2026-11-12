/**
 * Покрытие Admin API экранами консоли (UI-02): «МЕТОД путь» операции OpenAPI → файл экрана в `src/`.
 * Операция без экрана — строка в `coverage-pending.txt`; тест падает, если операция схемы не учтена
 * ни здесь, ни там (критерий Ф2: список ожидания пуст).
 */
export const covered: Record<string, string> = {
  "POST /api/v1/session": "pages/login.tsx",
  "GET /api/v1/whoami": "components/header.tsx",
  "GET /api/v1/orgs/{org}": "pages/organization.tsx",
  "PUT /api/v1/orgs/{org}/policy": "pages/organization.tsx",
  "GET /api/v1/orgs/{org}/domains": "pages/domains.tsx",
  "POST /api/v1/orgs/{org}/domains": "pages/domains.tsx",
  "GET /api/v1/orgs/{org}/domains/{domain}": "pages/domains.tsx",
  "DELETE /api/v1/orgs/{org}/domains/{domain}": "pages/domains.tsx",
  "PUT /api/v1/orgs/{org}/domains/{domain}/policy": "pages/domains.tsx",
  "GET /api/v1/orgs/{org}/accounts": "pages/accounts.tsx",
  "POST /api/v1/orgs/{org}/accounts": "pages/accounts.tsx",
  "GET /api/v1/orgs/{org}/accounts/{account}": "pages/accounts.tsx",
  "PUT /api/v1/orgs/{org}/accounts/{account}/policy": "pages/accounts.tsx",
  "GET /api/v1/orgs/{org}/accounts/{account}/app-passwords": "pages/accounts.tsx",
  "POST /api/v1/orgs/{org}/accounts/{account}/app-passwords": "pages/accounts.tsx",
  "DELETE /api/v1/orgs/{org}/accounts/{account}/app-passwords/{id}": "pages/accounts.tsx",
  "GET /api/v1/orgs/{org}/groups": "pages/groups.tsx",
  "POST /api/v1/orgs/{org}/groups": "pages/groups.tsx",
  "GET /api/v1/orgs/{org}/groups/{group}": "pages/groups.tsx",
  "PATCH /api/v1/orgs/{org}/groups/{group}": "pages/groups.tsx",
  "DELETE /api/v1/orgs/{org}/groups/{group}": "pages/groups.tsx",
  "GET /api/v1/orgs/{org}/moderation": "components/moderation-queue.tsx",
  "POST /api/v1/orgs/{org}/moderation/{held}/approve": "components/moderation-queue.tsx",
  "POST /api/v1/orgs/{org}/moderation/{held}/reject": "components/moderation-queue.tsx",
};
