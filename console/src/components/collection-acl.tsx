// Общий доступ к календарям и адресным книгам учётной записи (ADM-16): права субъектов — имя входа, имя группы или
// «все» (`anyone`). Права субъекта заменяются целиком; отзыв — пустые права. Сверх политики организации узел отказывает.
import { type HttpError, useCustom, useCustomMutation } from "@refinedev/core";
import { AutoComplete, Button, Empty, Form, Popconfirm, Select, Table } from "antd";
import { useState } from "react";

import type { Schemas } from "../api/client";
import { useReadOnly } from "../providers/access";
import { NO_RIGHTS, type Right, RightsChecks, RightsTags, toRights } from "./rights";

type Collection = Schemas["CollectionInfo"];
type Entry = Schemas["AclEntryBody"];
type Values = { identifier: string; rights: Right[] };

/** Список ответа; до ответа и при ошибке узла — пусто. */
const rows = <T,>(data: unknown): T[] => (Array.isArray(data) ? (data as T[]) : []);

const KINDS: Record<Schemas["CollectionKindBody"], string> = { calendar: "Календарь", address_book: "Адресная книга" };

function Acl({ account, collection }: { account: string; collection: string }) {
  const readOnly = useReadOnly("accounts");
  const url = `/api/v1/orgs/{org}/accounts/${encodeURIComponent(account)}/collections/${encodeURIComponent(collection)}/acl`;
  const { result, query } = useCustom<Entry[]>({ url, method: "get" });
  const { mutate, mutation } = useCustomMutation<Entry[], HttpError, Schemas["RightsBody"]>();
  const [form] = Form.useForm<Values>();
  const put = (identifier: string, rights: Schemas["RightsBody"], message: string) =>
    mutate(
      { url: `${url}/${encodeURIComponent(identifier.trim())}`, method: "put", values: rights, successNotification: { type: "success", message } },
      {
        onSuccess: () => {
          form.resetFields();
          void query.refetch();
        },
      },
    );
  return (
    <>
      {!readOnly && (
        <Form<Values>
          form={form}
          layout="inline"
          initialValues={{ rights: ["read"] }}
          style={{ marginBottom: 16 }}
          onFinish={(v) => put(v.identifier, toRights(v.rights), "Права сохранены")}
        >
          <Form.Item
            name="identifier"
            label="Кому"
            rules={[{ required: true, whitespace: true, message: "Имя входа, группа или anyone" }]}
          >
            <AutoComplete style={{ width: 260 }} placeholder="user@example.org, группа, anyone" options={[{ value: "anyone" }]} />
          </Form.Item>
          <Form.Item name="rights" label="Права" rules={[{ required: true, message: "Выберите права" }]}>
            <RightsChecks />
          </Form.Item>
          <Button type="primary" htmlType="submit" loading={mutation.isPending}>
            Выдать
          </Button>
        </Form>
      )}
      <Table<Entry> dataSource={rows<Entry>(result.data)} loading={query.isLoading} rowKey="identifier" pagination={false} size="small">
        <Table.Column<Entry> title="Субъект" render={(_, e) => (e.identifier === "anyone" ? "все (anyone)" : e.identifier)} />
        <Table.Column<Entry> title="Права" render={(_, e) => <RightsTags rights={e.rights} />} />
        {!readOnly && (
          <Table.Column<Entry>
            title="Действия"
            width={140}
            render={(_, e) => (
              <Popconfirm title={`Отозвать права ${e.identifier}?`} okText="Отозвать" onConfirm={() => put(e.identifier, NO_RIGHTS, "Права отозваны")}>
                <Button size="small" danger>
                  Отозвать
                </Button>
              </Popconfirm>
            )}
          />
        )}
      </Table>
    </>
  );
}

/** Календари и адресные книги учётной записи и права на выбранную. */
export function CollectionsAcl({ account }: { account: string }) {
  const url = `/api/v1/orgs/{org}/accounts/${encodeURIComponent(account)}/collections`;
  const { result, query } = useCustom<Collection[]>({ url, method: "get" });
  const [chosen, setChosen] = useState<string>();
  const list = rows<Collection>(result.data);
  if (query.isLoading) return null;
  if (!list.length) return <Empty description="Календарей и адресных книг нет" />;
  const current = chosen ?? list[0].id;
  return (
    <>
      <Select
        aria-label="Календарь или адресная книга"
        value={current}
        onChange={setChosen}
        style={{ width: 420, marginBottom: 16 }}
        options={list.map((c) => ({ value: c.id, label: `${KINDS[c.kind]}: ${c.display_name} (${c.name})` }))}
      />
      <Acl key={current} account={account} collection={current} />
    </>
  );
}
