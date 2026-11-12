import { Create, List, Show, ShowButton, useForm } from "@refinedev/antd";
import { useCreate, useCustom, useCustomMutation, useDelete, useInfiniteList, useList, useShow } from "@refinedev/core";
import { Alert, Button, Descriptions, Divider, Form, Input, Modal, Popconfirm, Space, Table, Tag, Typography } from "antd";
import { useState } from "react";
import { Link } from "react-router";

import type { Schemas } from "../api/client";
import { bytes, when } from "../components/format";
import { PolicyForm } from "../components/policy-form";

type Account = Schemas["AccountInfo"];
type AppPassword = Schemas["AppPasswordInfo"];

const state = (a: Account) => (a.enabled ? <Tag color="green">Вход разрешён</Tag> : <Tag color="red">Заблокирована</Tag>);
const quota = (a: Account) =>
  a.quota.limit_bytes == null ? bytes(a.quota.used_bytes) : `${bytes(a.quota.used_bytes)} из ${bytes(a.quota.limit_bytes)}`;

/** Учётные записи: поиск по началу имени входа, страницы по курсору (размер страницы задаёт узел). */
export function AccountList() {
  const [prefix, setPrefix] = useState("");
  const { query } = useInfiniteList<Account>({
    resource: "accounts",
    pagination: { mode: "off" },
    filters: prefix ? [{ field: "prefix", operator: "eq", value: prefix }] : [],
    queryOptions: { getNextPageParam: (last) => last.cursor?.next },
  });
  return (
    <List>
      <Input.Search
        aria-label="Поиск по имени входа"
        placeholder="Начало имени входа"
        allowClear
        enterButton="Найти"
        onSearch={(v) => setPrefix(v.trim())}
        style={{ maxWidth: 420, marginBottom: 16 }}
      />
      <Table<Account>
        dataSource={query.data?.pages.flatMap((p) => p.data)}
        loading={query.isLoading}
        rowKey="id"
        pagination={false}
      >
        <Table.Column dataIndex="login" title="Имя входа" />
        <Table.Column dataIndex="display_name" title="Отображаемое имя" />
        <Table.Column<Account> title="Источник" render={(_, a) => (a.external_id ? "каталог" : "локальная")} />
        <Table.Column<Account> title="Состояние" render={(_, a) => state(a)} />
        <Table.Column<Account> title="Занято" render={(_, a) => quota(a)} />
        <Table.Column<Account> title="Действия" width={140} render={(_, a) => <ShowButton size="small" recordItemId={a.id} />} />
      </Table>
      {query.hasNextPage && (
        <Button onClick={() => void query.fetchNextPage()} loading={query.isFetchingNextPage} style={{ marginTop: 16 }}>
          Показать ещё
        </Button>
      )}
    </List>
  );
}

/** Новая локальная учётная запись; пароль — пароль приложения на карточке записи. */
export function AccountCreate() {
  const { formProps, saveButtonProps } = useForm<Account, never, Schemas["NewAccount"]>({ redirect: "show" });
  return (
    <Create saveButtonProps={saveButtonProps}>
      <Form {...formProps} layout="vertical">
        <Form.Item
          name="login"
          label="Имя входа"
          extra="Адрес в принятом домене организации; приводится к нижнему регистру."
          rules={[{ required: true, message: "Укажите имя входа" }]}
        >
          <Input placeholder="user@example.org" />
        </Form.Item>
        <Form.Item name="display_name" label="Отображаемое имя" extra="Не задано — имя входа.">
          <Input />
        </Form.Item>
      </Form>
    </Create>
  );
}

/** Пароли приложений учётной записи: список (с отозванными), выпуск, отзыв. */
function AppPasswords({ account }: { account: string }) {
  const resource = `accounts/${encodeURIComponent(account)}/app-passwords`;
  const { result, query } = useList<AppPassword>({ resource, pagination: { mode: "off" } });
  const { mutate: issue, mutation: issuing } = useCreate<Schemas["IssuedAppPassword"]>();
  const { mutate: revoke } = useDelete();
  const [issued, setIssued] = useState<Schemas["IssuedAppPassword"]>();
  const [form] = Form.useForm<Schemas["NewAppPassword"]>();
  return (
    <>
      <Form
        form={form}
        layout="inline"
        style={{ marginBottom: 16 }}
        onFinish={(values) =>
          issue(
            { resource, values, successNotification: false },
            {
              onSuccess: (r) => {
                setIssued(r.data);
                form.resetFields();
              },
            },
          )
        }
      >
        <Form.Item name="label" label="Подпись" rules={[{ max: 255, message: "Не длиннее 255 символов" }]}>
          <Input placeholder="Телефон" style={{ width: 280 }} />
        </Form.Item>
        <Button type="primary" htmlType="submit" loading={issuing.isPending}>
          Выпустить пароль
        </Button>
      </Form>
      <Table<AppPassword> dataSource={result.data} loading={query.isLoading} rowKey="id" pagination={false} size="small">
        <Table.Column dataIndex="label" title="Подпись" />
        <Table.Column<AppPassword> title="Выпущен" render={(_, p) => when(p.created_at)} />
        <Table.Column<AppPassword>
          title="Состояние"
          render={(_, p) => (p.revoked_at ? <Tag>Отозван {when(p.revoked_at)}</Tag> : <Tag color="green">Действует</Tag>)}
        />
        <Table.Column<AppPassword>
          title="Действия"
          width={140}
          render={(_, p) =>
            p.revoked_at ? null : (
              <Popconfirm
                title={`Отозвать пароль «${p.label}»?`}
                okText="Отозвать"
                onConfirm={() =>
                  revoke({ resource, id: p.id, successNotification: { type: "success", message: "Пароль отозван" } })
                }
              >
                <Button size="small" danger>
                  Отозвать
                </Button>
              </Popconfirm>
            )
          }
        />
      </Table>
      {/* Окно снимается с экрана сразу: значение пароля не остаётся в странице после закрытия. */}
      {issued && (
        <Modal
          open
          title={`Пароль приложения «${issued.label}»`}
          onCancel={() => setIssued(undefined)}
          footer={<Button onClick={() => setIssued(undefined)}>Закрыть</Button>}
          maskClosable={false}
        >
          <Alert type="warning" showIcon message="Пароль показывается один раз: после закрытия окна узнать его нельзя." />
          <Typography.Paragraph copyable code style={{ fontSize: 18, marginTop: 16 }}>
            {issued.password}
          </Typography.Paragraph>
        </Modal>
      )}
    </>
  );
}

/** Учётная запись: сведения, политика ящика, пароли приложений. */
export function AccountShow() {
  const { result: account, query } = useShow<Account>();
  const { mutate, mutation } = useCustomMutation<Schemas["PolicyBody"]>();
  // Узел отдаёт идентификатор основного домена: имя — из принятых доменов организации (транспорт, единственный
  // ресурс доменов); список недоступен — показывается идентификатор, без уведомления об ошибке.
  const { result: accepted } = useCustom<{ version: number; domains: { id: string; name: string }[] }>({
    url: "/api/v1/orgs/{org}/transport/domains",
    method: "get",
    errorNotification: false,
  });
  const domain = accepted.data?.domains?.find((d) => d.id === account?.domain)?.name ?? account?.domain;
  return (
    <Show title={account?.login ?? "Учётная запись"} isLoading={query.isLoading}>
      <Descriptions column={1}>
        <Descriptions.Item label="Имя входа">{account?.login}</Descriptions.Item>
        <Descriptions.Item label="Отображаемое имя">{account?.display_name}</Descriptions.Item>
        <Descriptions.Item label="Основной домен">{domain}</Descriptions.Item>
        <Descriptions.Item label="Источник">
          {account?.external_id ? `каталог (${account.external_id})` : "локальная запись"}
        </Descriptions.Item>
        <Descriptions.Item label="Состояние">{account && state(account)}</Descriptions.Item>
        <Descriptions.Item label="Занято">{account && `${quota(account)}, писем: ${account.quota.messages}`}</Descriptions.Item>
        <Descriptions.Item label="Идентификатор">{account?.id}</Descriptions.Item>
      </Descriptions>
      {account && (
        <Space size="large" style={{ marginTop: 8 }}>
          <Link to={`/accounts/${encodeURIComponent(account.id)}/trash`}>Корзина и скрытые письма</Link>
          <Link to={`/accounts/${encodeURIComponent(account.id)}/sieve`}>Правила и автоответ</Link>
        </Space>
      )}
      <Divider orientation="left">Политика ящика</Divider>
      <PolicyForm
        policies={account}
        saving={mutation.isPending}
        onSave={(values) =>
          account &&
          mutate(
            {
              url: `/api/v1/orgs/{org}/accounts/${encodeURIComponent(account.id)}/policy`,
              method: "put",
              values,
              successNotification: { type: "success", message: "Политика ящика сохранена" },
            },
            { onSuccess: () => void query.refetch() },
          )
        }
      />
      <Divider orientation="left">Пароли приложений</Divider>
      {account && <AppPasswords account={account.id} />}
    </Show>
  );
}
