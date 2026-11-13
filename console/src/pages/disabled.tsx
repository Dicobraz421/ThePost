// Отключённые ящики (SRS 4.7, ADM-10): учётные записи, отключённые каталогом, хранятся до срока; ящик можно
// восстановить или подключить к другой учётной записи (её вход и адрес переходят к ящику).
import { List } from "@refinedev/antd";
import { type HttpError, useCustomMutation, useList } from "@refinedev/core";
import { Button, Form, Modal, Popconfirm, Select, Space, Table, Tag } from "antd";
import { useState } from "react";

import type { Schemas } from "../api/client";
import { AccountPick } from "../components/account-select";
import { when } from "../components/format";
import { useReadOnly } from "../providers/access";

type Disabled = Schemas["DisabledInfo"];
type Attach = { to: string; existing: Schemas["ExistingMode"] | "empty" };

const EXISTING: Record<Attach["existing"], string> = {
  empty: "ящик учётной записи пуст (иначе отказ)",
  keep: "оставить письма её ящика (keep)",
  delete: "удалить письма её ящика (delete)",
};

const BASE = "/api/v1/orgs/{org}/disabled-accounts";

function AttachModal(p: { box: Disabled; saving: boolean; onSave: (v: Attach) => void; onClose: () => void }) {
  const [form] = Form.useForm<Attach>();
  return (
    <Modal open title={`Подключить ящик ${p.box.login}`} okText="Подключить" confirmLoading={p.saving} onOk={() => form.submit()} onCancel={p.onClose}>
      <Form<Attach> form={form} layout="vertical" initialValues={{ existing: "empty" }} onFinish={p.onSave}>
        <Form.Item
          name="to"
          label="Учётная запись"
          extra="Её запись удаляется: вход и адрес переходят к подключаемому ящику."
          rules={[{ required: true, message: "Выберите учётную запись" }]}
        >
          <AccountPick />
        </Form.Item>
        <Form.Item name="existing" label="Письма ящика учётной записи">
          <Select options={Object.entries(EXISTING).map(([value, label]) => ({ value, label }))} />
        </Form.Item>
      </Form>
    </Modal>
  );
}

export function DisabledPage() {
  const readOnly = useReadOnly("disabled-accounts");
  const { result, query } = useList<Disabled>({ resource: "disabled-accounts", pagination: { mode: "off" } });
  const { mutate, mutation } = useCustomMutation<Schemas["AccountInfo"], HttpError, Schemas["AttachBody"] | undefined>();
  const [attaching, setAttaching] = useState<Disabled>();
  const run = (box: Disabled, action: "restore" | "attach", values?: Schemas["AttachBody"]) =>
    mutate(
      {
        url: `${BASE}/${encodeURIComponent(box.id)}/${action}`,
        method: "post",
        values,
        successNotification: { type: "success", message: action === "restore" ? "Ящик восстановлен" : "Ящик подключён" },
      },
      {
        onSuccess: () => {
          setAttaching(undefined);
          void query.refetch();
        },
      },
    );
  return (
    <List title="Отключённые ящики">
      <Table<Disabled> dataSource={result.data} loading={query.isLoading} rowKey="id" pagination={false}>
        <Table.Column dataIndex="login" title="Имя входа" />
        <Table.Column dataIndex="display_name" title="Отображаемое имя" />
        <Table.Column<Disabled> title="Источник" render={(_, d) => (d.external_id ? <Tag>каталог</Tag> : "локальная")} />
        <Table.Column<Disabled> title="Хранится до" render={(_, d) => when(d.until)} />
        {!readOnly && (
          <Table.Column<Disabled>
            title="Действия"
            width={280}
            render={(_, d) => (
              <Space>
                <Popconfirm title={`Восстановить ящик ${d.login}?`} okText="Восстановить" onConfirm={() => run(d, "restore")}>
                  <Button size="small">Восстановить</Button>
                </Popconfirm>
                <Button size="small" onClick={() => setAttaching(d)}>
                  Подключить…
                </Button>
              </Space>
            )}
          />
        )}
      </Table>
      {attaching && (
        <AttachModal
          box={attaching}
          saving={mutation.isPending}
          onClose={() => setAttaching(undefined)}
          onSave={(v) => run(attaching, "attach", { to: v.to, existing: v.existing === "empty" ? null : v.existing })}
        />
      )}
    </List>
  );
}
