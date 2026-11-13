// Динамическая группа (ADM-10): состав по фильтру атрибутов и подразделений вместо перечня участников.
// Фильтр — дерево условий (`FilterBody.filter`): правится текстом JSON, узел проверяет его при сохранении.
import { type HttpError, useCustom, useCustomMutation, useList } from "@refinedev/core";
import { Alert, Button, Form, Input, Popconfirm, Space, Switch, Typography } from "antd";
import { useState } from "react";
import { Link } from "react-router";

import type { Schemas } from "../api/client";
import { useReadOnly } from "../providers/access";

type Filter = Schemas["FilterBody"];
type Values = { filter: string; contacts: boolean };

const EXAMPLE = { all: [{ equals: { attribute: "department", value: "Продажи" } }, { unit: { unit: "UUID подразделения", subtree: true } }] };
const pretty = (v: unknown) => JSON.stringify(v, null, 2);

/** Текст фильтра → объект (в схеме — произвольный объект); не объект JSON — ошибка проверки поля. */
function parse(text: string): Filter["filter"] {
  const v: unknown = JSON.parse(text);
  if (!v || typeof v !== "object" || Array.isArray(v)) throw new Error("не объект");
  return v as Filter["filter"];
}

function Members({ url }: { url: string }) {
  const { result } = useCustom<Schemas["DynamicMembers"]>({ url: `${url}/members`, method: "get" });
  const { result: contacts } = useList<Schemas["ContactInfo"]>({ resource: "contacts", pagination: { mode: "off" } });
  const m = result.data;
  if (!m?.accounts) return null;
  const name = (id: string) => contacts.data.find((c) => c.id === id)?.email ?? id;
  return (
    <Typography.Paragraph>
      Текущий состав: учётных записей — {m.accounts.length}, контактов — {m.contacts.length}.
      {m.accounts.length > 0 && (
        <>
          {" "}
          Учётные записи:{" "}
          {m.accounts.map((a, i) => (
            <span key={a}>
              {i > 0 && ", "}
              <Link to={`/accounts/${encodeURIComponent(a)}`}>{a}</Link>
            </span>
          ))}
          .
        </>
      )}
      {m.contacts.length > 0 && ` Контакты: ${m.contacts.map(name).join(", ")}.`}
    </Typography.Paragraph>
  );
}

/** `members` — статических участников у группы: динамической такая группа стать не может (узел ответит 409). */
export function DynamicFilter({ group, members }: { group: string; members: number }) {
  const readOnly = useReadOnly("groups");
  const url = `/api/v1/orgs/{org}/groups/${encodeURIComponent(group)}/filter`;
  // Группа не динамическая — 404: это ответ, а не ошибка экрана.
  const { result, query } = useCustom<Filter>({ url, method: "get", errorNotification: false, queryOptions: { retry: false } });
  const { mutate, mutation } = useCustomMutation<Filter, HttpError, Filter | undefined>();
  const [editing, setEditing] = useState(false);
  const current = query.isError ? undefined : result.data;
  if (query.isLoading) return null;
  const refetch = { onSuccess: () => void query.refetch().then(() => setEditing(false)) };
  if (!current?.filter && !editing) {
    return (
      <Space direction="vertical">
        <Typography.Text>Группа статическая: состав — участники выше.</Typography.Text>
        {!readOnly &&
          (members > 0 ? (
            <Typography.Text type="secondary">Чтобы задать фильтр состава, уберите статических участников.</Typography.Text>
          ) : (
            <Button onClick={() => setEditing(true)}>Задать фильтр состава</Button>
          ))}
      </Space>
    );
  }
  return (
    <>
      {current?.filter && <Members url={url} />}
      <Form<Values>
        layout="vertical"
        disabled={readOnly}
        initialValues={{ filter: pretty(current?.filter ?? EXAMPLE), contacts: current?.contacts ?? false }}
        onFinish={(v) =>
          mutate(
            {
              url,
              method: "put",
              values: { filter: parse(v.filter), contacts: v.contacts },
              successNotification: { type: "success", message: "Фильтр состава сохранён" },
            },
            refetch,
          )
        }
      >
        <Alert
          type="info"
          showIcon
          style={{ marginBottom: 16 }}
          message="Условия: equals, prefix, present (атрибуты), unit (подразделение), all, any, not."
        />
        <Form.Item
          name="filter"
          label="Фильтр состава (JSON)"
          rules={[
            {
              validator: async (_, text: string) => {
                try {
                  parse(text);
                } catch {
                  throw new Error("Фильтр — объект JSON");
                }
              },
            },
          ]}
        >
          <Input.TextArea autoSize={{ minRows: 4, maxRows: 16 }} style={{ fontFamily: "monospace" }} />
        </Form.Item>
        <Form.Item name="contacts" label="Включать почтовые контакты" valuePropName="checked">
          <Switch />
        </Form.Item>
        {!readOnly && (
          <Space>
            <Button type="primary" htmlType="submit" loading={mutation.isPending}>
              Сохранить фильтр
            </Button>
            {current?.filter ? (
              <Popconfirm
                title="Снять фильтр? Группа станет статической без участников."
                okText="Снять"
                onConfirm={() =>
                  mutate({ url, method: "delete", values: undefined, successNotification: { type: "success", message: "Фильтр снят" } }, refetch)
                }
              >
                <Button danger>Снять фильтр</Button>
              </Popconfirm>
            ) : (
              <Button onClick={() => setEditing(false)}>Отмена</Button>
            )}
          </Space>
        )}
      </Form>
    </>
  );
}
