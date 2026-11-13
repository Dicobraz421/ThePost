// Аудит доступа к ящикам (SRS 4.7, ADM-10): выборка журнала по ящику, исполнителю, действию и интервалу;
// политика аудита организации (политика ящика — на карточке учётной записи).
import { List } from "@refinedev/antd";
import { type HttpError, useCustomMutation } from "@refinedev/core";
import { Button, DatePicker, Divider, Form, Input, Select, Table, Typography } from "antd";
import type { Dayjs } from "dayjs";
import { useState } from "react";
import { useSearchParams } from "react-router";

import type { Schemas } from "../api/client";
import { AuditPolicyForm, OWNER_ACTIONS } from "../components/audit-policy-form";
import { useCursorPages } from "../components/cursor-pages";
import { interval, moment } from "../components/format";
import { useReadOnly } from "../providers/access";

type Entry = Schemas["MailboxAuditEntry"];
type Filters = { account?: string; actor?: string; actor_kind?: string; action?: string; range?: [Dayjs | null, Dayjs | null] | null };

const BASE = "/api/v1/orgs/{org}/mailbox-audit";
const KINDS: Record<string, string> = { owner: "владелец", delegate: "представитель", admin: "администратор" };
const ACTIONS: Record<string, string> = {
  login: "вход",
  read: "чтение",
  ...OWNER_ACTIONS,
  send_as: "отправка от имени (send as)",
  send_on_behalf: "отправка по поручению (on behalf)",
  change_rights: "изменение прав",
};
const options = (m: Record<string, string>) => Object.entries(m).map(([value, label]) => ({ value, label }));

/** Политика организации: узел её отдельно не отдаёт — форма задаёт её заново целиком. */
function OrgPolicy() {
  const { mutate, mutation } = useCustomMutation<Schemas["AuditPolicyBody"], HttpError, Schemas["AuditPolicyBody"] | undefined>();
  const url = `${BASE}/policy`;
  return (
    <>
      <Divider orientation="left">Политика аудита организации</Divider>
      <Typography.Paragraph type="secondary">
        Действует для ящиков без своей политики. Сохранение заменяет политику организации целиком; действующая политика ящика —
        на карточке учётной записи. После сброса записываются только действия не владельца.
      </Typography.Paragraph>
      <AuditPolicyForm
        readOnly={false}
        saving={mutation.isPending}
        onSave={(values) =>
          mutate({ url, method: "put", values, successNotification: { type: "success", message: "Политика аудита организации сохранена" } })
        }
        reset={{
          label: "Сбросить к политике по умолчанию",
          confirm: "Сбросить политику аудита организации?",
          run: () =>
            mutate({
              url,
              method: "delete",
              values: undefined,
              successNotification: { type: "success", message: "Политика аудита организации сброшена" },
            }),
        }}
      />
    </>
  );
}

export function MailboxAuditPage() {
  const [params] = useSearchParams();
  const [filters, setFilters] = useState<Filters>({ account: params.get("account") ?? undefined });
  const readOnly = useReadOnly("mailbox-audit");
  const { page, loading, pager } = useCursorPages<Schemas["MailboxAuditPage"]>(BASE, {
    account: filters.account?.trim(),
    actor: filters.actor?.trim(),
    actor_kind: filters.actor_kind,
    action: filters.action,
    ...interval(filters.range),
  });
  return (
    <List title="Аудит ящиков" breadcrumb={false} headerButtons={() => null}>
      <Form<Filters> layout="inline" initialValues={filters} onFinish={setFilters} style={{ marginBottom: 16, rowGap: 8 }}>
        <Form.Item name="account" label="Ящик">
          <Input allowClear placeholder="UUID учётной записи" style={{ width: 320 }} />
        </Form.Item>
        <Form.Item name="actor" label="Исполнитель">
          <Input allowClear placeholder="имя входа" />
        </Form.Item>
        <Form.Item name="actor_kind" label="Кто">
          <Select allowClear options={options(KINDS)} style={{ width: 180 }} />
        </Form.Item>
        <Form.Item name="action" label="Действие">
          <Select allowClear options={options(ACTIONS)} style={{ width: 260 }} />
        </Form.Item>
        <Form.Item name="range" label="Интервал">
          <DatePicker.RangePicker showTime allowEmpty={[true, true]} />
        </Form.Item>
        <Button type="primary" htmlType="submit">
          Найти
        </Button>
      </Form>
      <Table<Entry>
        dataSource={page?.entries}
        loading={loading}
        rowKey={(e) => `${e.at} ${e.account} ${e.actor} ${e.action} ${e.message}`}
        pagination={false}
      >
        <Table.Column<Entry> title="Время" render={(_, e) => moment(e.at)} />
        <Table.Column dataIndex="account" title="Ящик" />
        <Table.Column dataIndex="actor" title="Исполнитель" />
        <Table.Column<Entry> title="Кто" render={(_, e) => KINDS[e.actor_kind] ?? e.actor_kind} />
        <Table.Column<Entry> title="Действие" render={(_, e) => ACTIONS[e.action] ?? e.action} />
        <Table.Column<Entry> title="Папка" render={(_, e) => e.mailbox ?? "—"} />
        <Table.Column<Entry> title="Письмо" render={(_, e) => e.message ?? "—"} />
        <Table.Column<Entry> title="Клиент" render={(_, e) => e.client ?? "—"} />
        <Table.Column<Entry> title="Пояснение" render={(_, e) => e.detail ?? "—"} />
      </Table>
      {pager}
      {!readOnly && <OrgPolicy />}
    </List>
  );
}
