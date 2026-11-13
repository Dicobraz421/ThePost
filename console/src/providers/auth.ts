// Вход в консоль (SRS 5.2, решение 117): пароль администратора, OIDC с PKCE или — при выключенном входе по паролю —
// одноразовый токен `thepost admin init` (META-43).
import type { AuthProvider } from "@refinedev/core";

import { API, getOrg, loadSession, request, saveSession, type Schemas, setOrg, whoami } from "../api/client";

/** Способы входа узла (`GET /api/v1/session`): поставщик OIDC, если настроен, и вход по паролю. */
export type SessionInfo = Schemas["SessionInfo"];
type Oidc = Schemas["OidcLogin"];
export type LoginParams = Schemas["PasswordLogin"] | { token: string } | { oidc: Oidc; callback?: boolean };

const redirectUri = () => `${location.origin}${import.meta.env.BASE_URL}login`;

/** Способы входа узла; узел недоступен — форма пароля (ошибка придёт при входе). */
export async function sessionInfo(): Promise<SessionInfo> {
  try {
    return (await request<SessionInfo>(`${API}/session`)).data;
  } catch {
    return { oidc: null, password: true };
  }
}

async function oidcManager(p: Oidc) {
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

/** Сессия Admin API по паролю или токену `admin init` (`POST /api/v1/session`). */
async function openSession(o: { body?: Schemas["PasswordLogin"]; bearer?: string }) {
  const { data } = await request<Schemas["AdminSession"]>(`${API}/session`, { method: "POST", ...o });
  saveSession({ token: data.session, expires_at: data.expires_at });
  if (!getOrg()) setOrg(data.tenant);
}

async function login(p: LoginParams) {
  if ("password" in p) return openSession({ body: { login: p.login.trim(), password: p.password } });
  if ("token" in p) return openSession({ bearer: p.token.trim() });
  const manager = await oidcManager(p.oidc);
  if (!p.callback) return manager.signinRedirect();
  const user = await manager.signinRedirectCallback();
  await manager.removeUser();
  saveSession({ token: user.access_token, expires_at: new Date((user.expires_at ?? 0) * 1000).toISOString() });
  if (!getOrg()) setOrg((await whoami()).tenant);
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
  getIdentity: whoami,
  getPermissions: async () => (await whoami()).roles,
};
