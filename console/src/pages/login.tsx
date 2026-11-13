import { useLogin } from "@refinedev/core";
import { Button, Card, Divider, Form, Input, Layout, Typography } from "antd";
import { useEffect, useState } from "react";

import type { Schemas } from "../api/client";
import { type LoginParams, type SessionInfo, sessionInfo } from "../providers/auth";

/**
 * Экран входа (решение 117): OIDC с PKCE, если узел сообщает поставщика; имя входа и пароль администратора, если вход
 * по паролю включён (`admin.password_login`); иначе — одноразовый токен `thepost admin init` (META-43).
 */
export function LoginPage() {
  const { mutate: login, isPending } = useLogin<LoginParams>();
  const [info, setInfo] = useState<SessionInfo | null>(null);
  useEffect(() => {
    void sessionInfo().then((i) => {
      setInfo(i);
      // Возврат от поставщика OIDC: в адресе — код авторизации.
      const q = new URLSearchParams(location.search);
      if (i.oidc && q.has("code") && q.has("state")) login({ oidc: i.oidc, callback: true });
    });
  }, [login]);
  const oidc = info?.oidc;
  const submit = (
    <Button type={oidc ? "default" : "primary"} htmlType="submit" loading={isPending} block>
      Войти
    </Button>
  );
  return (
    <Layout style={{ minHeight: "100vh", justifyContent: "center", alignItems: "center" }}>
      <Card style={{ width: 400 }}>
        <Typography.Title level={3}>The Post — вход</Typography.Title>
        {oidc && (
          <>
            <Button type="primary" block loading={isPending} onClick={() => login({ oidc })}>
              Войти через OIDC
            </Button>
            <Divider plain>или</Divider>
          </>
        )}
        {info?.password && (
          <Form layout="vertical" onFinish={(v: Schemas["PasswordLogin"]) => login(v)}>
            <Form.Item name="login" label="Имя входа" rules={[{ required: true, message: "Укажите имя входа" }]}>
              <Input autoComplete="username" />
            </Form.Item>
            <Form.Item
              name="password"
              label="Пароль"
              extra="Пароль администратора; первый — выдаёт thepost admin init."
              rules={[{ required: true, message: "Укажите пароль" }]}
            >
              <Input.Password autoComplete="current-password" />
            </Form.Item>
            {submit}
          </Form>
        )}
        {info && !info.password && (
          <Form layout="vertical" onFinish={(v: { token: string }) => login(v)}>
            <Form.Item
              name="token"
              label="Токен администратора"
              extra="Вход по паролю выключен. Токен выдаёт thepost admin init, действует один раз."
              rules={[{ required: true, message: "Укажите токен" }]}
            >
              <Input.Password autoComplete="off" />
            </Form.Item>
            {submit}
          </Form>
        )}
      </Card>
    </Layout>
  );
}
