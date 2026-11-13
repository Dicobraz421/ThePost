import { useLogin } from "@refinedev/core";
import { Button, Card, Divider, Form, Input, Layout, Typography } from "antd";
import { useEffect, useState } from "react";

import type { Schemas } from "../api/client";
import { type LoginParams, sessionInfo } from "../providers/auth";

/** Экран входа: одноразовый токен `thepost admin init` (META-43) и OIDC с PKCE, если узел сообщает поставщика. */
export function LoginPage() {
  const { mutate: login, isPending } = useLogin<LoginParams>();
  const [oidc, setOidc] = useState<Schemas["OidcLogin"] | null>(null);
  useEffect(() => {
    void sessionInfo().then((info) => {
      setOidc(info.oidc ?? null);
      // Возврат от поставщика OIDC: в адресе — код авторизации.
      const q = new URLSearchParams(location.search);
      if (info.oidc && q.has("code") && q.has("state")) login({ oidc: info.oidc, callback: true });
    });
  }, [login]);
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
        <Form layout="vertical" onFinish={(v: { token: string }) => login(v)}>
          <Form.Item
            name="token"
            label="Токен администратора"
            extra="Выдаётся командой thepost admin init, действует один раз."
            rules={[{ required: true, message: "Укажите токен" }]}
          >
            <Input.Password autoComplete="off" />
          </Form.Item>
          <Button type={oidc ? "default" : "primary"} htmlType="submit" loading={isPending} block>
            Войти
          </Button>
        </Form>
      </Card>
    </Layout>
  );
}
