// Вход в консоль (SRS 5.2): одноразовый токен `thepost admin init` (META-43) или OIDC с PKCE.
import type { AuthProvider } from "@refinedev/core";

import { API, getOrg, loadSession, request, saveSession, type Schemas, setOrg } from "../api/client";

/**
 * Ответ `GET /api/v1/session`: поставщик OIDC, если настроен. Операции в схеме узла пока нет —
 * форма ответа предложена консолью (см. «Решения для владельца» в PR console-01).
 */
export type SessionInfo = { oidc?: { issuer: string; client_id: string; scope?: string } };
export type LoginParams = { token: string } | { oidc: NonNullable<SessionInfo["oidc"]>; callback?: boolean };

const redirectUri = () => `${location.origin}${import.meta.env.BASE_URL}login`;

/** Способы входа узла; ошибка или отсутствие операции — только токен. */
export async function sessionInfo(): Promise<SessionInfo> {
  try {
    return (await request<SessionInfo>(`${API}/session`)).data ?? {};
  } catch {
    return {};
  }
}

async function oidcManager(p: NonNullable<SessionInfo["oidc"]>) {
  const { UserManager, WebStorageStateStore } = await import("oidc-client-ts");
  return new UserManager({
    authority: p.issuer,
    client_id: p.client_id,
    scope: p.scope ?? "openid",
    redirect_uri: redirectUri(),
    response_type: "code", // код авторизации с PKCE
    userStore: new WebStorageStateStore({ store: sessionStorage }),
  });
}

async function login(p: LoginParams) {
  if ("token" in p) {
    const { data } = await request<Schemas["AdminSession"]>(`${API}/session`, { method: "POST", bearer: p.token.trim() });
    saveSession({ token: data.session, expires_at: data.expires_at });
    if (!getOrg()) setOrg(data.tenant);
    return;
  }
  const manager = await oidcManager(p.oidc);
  if (!p.callback) return manager.signinRedirect();
  const user = await manager.signinRedirectCallback();
  await manager.removeUser();
  saveSession({ token: user.access_token, expires_at: new Date((user.expires_at ?? 0) * 1000).toISOString() });
  if (!getOrg()) setOrg((await request<Schemas["WhoAmI"]>(`${API}/whoami`)).data.tenant);
}

export const authProvider: AuthProvider = {
  login: async (p: LoginParams) => {
    try {
      await login(p);
      return { success: true, redirectTo: "/" };
    } catch (e) {
      const message = (e as { message?: string }).message ?? "Вход не выполнен";
      return { success: false, error: { name: "Вход не выполнен", message } };
    }
  },
  logout: async () => {
    saveSession(null);
    return { success: true, redirectTo: "/login" };
  },
  // Истёкшая сессия не загружается: консоль возвращает на экран входа.
  check: async () => (loadSession() ? { authenticated: true } : { authenticated: false, logout: true, redirectTo: "/login" }),
  onError: async (e: { statusCode?: number }) => (e?.statusCode === 401 ? { logout: true, redirectTo: "/login" } : {}),
  getIdentity: async () => (await request<Schemas["WhoAmI"]>(`${API}/whoami`)).data,
  getPermissions: async () => (await request<Schemas["WhoAmI"]>(`${API}/whoami`)).data.roles,
};
