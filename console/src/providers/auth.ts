// Вход появится в UI-02: до него консоль показывает только экран входа.
import type { AuthProvider } from "@refinedev/core";

export const authProvider: AuthProvider = {
  login: async () => ({ success: false, error: new Error("Вход ещё не подключён (UI-02)") }),
  logout: async () => ({ success: true, redirectTo: "/login" }),
  check: async () => ({ authenticated: false, redirectTo: "/login" }),
  onError: async () => ({}),
};
