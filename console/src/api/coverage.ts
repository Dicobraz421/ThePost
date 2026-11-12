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
};
