import { useLogin } from "@refinedev/core";
import { Button, Card, Form, Input, Layout, Typography } from "antd";

/** Экран входа: одноразовый токен `thepost admin init` (META-43). */
export function LoginPage() {
  const { mutate: login, isPending } = useLogin<{ token: string }>();
  return (
    <Layout style={{ minHeight: "100vh", justifyContent: "center", alignItems: "center" }}>
      <Card style={{ width: 400 }}>
        <Typography.Title level={3}>The Post — вход</Typography.Title>
        <Form layout="vertical" onFinish={(v: { token: string }) => login(v)}>
          <Form.Item
            name="token"
            label="Токен администратора"
            extra="Выдаётся командой thepost admin init, действует один раз."
            rules={[{ required: true, message: "Укажите токен" }]}
          >
            <Input.Password autoComplete="off" />
          </Form.Item>
          <Button type="primary" htmlType="submit" loading={isPending} block>
            Войти
          </Button>
        </Form>
      </Card>
    </Layout>
  );
}
