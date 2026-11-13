// Журнал аудита (SRS 5.2): операции администраторов. Узел ищет по интервалу; администратор и действие —
// отбор среди записей загруженной страницы (таких условий у операции Admin API нет).
import { List } from "@refinedev/antd";
import { DatePicker, Form, Input, Table, Tag, Typography } from "antd";
import type { Dayjs } from "dayjs";
import { useState } from "react";

import type { Schemas } from "../api/client";
import { useCursorPages } from "../components/cursor-pages";
import { interval, moment } from "../components/format";

type Entry = Schemas["AuditEntry"];

// В записи журнала роли — строки (роль, которой уже нет, показывается как есть); подписи — у всех ролей схемы.
const ROLES: Partial<Record<string, string>> = {
  installation_admin: "администратор инсталляции",
  organization_admin: "администратор организации",
  operator: "оператор",
  auditor: "аудитор",
} satisfies Record<Schemas["AdminRole"], string>;
const has = (text: string, part: string) => text.toLowerCase().includes(part.trim().toLowerCase());

export function AuditPage() {
  const [range, setRange] = useState<[Dayjs | null, Dayjs | null] | null>(null);
  const [actor, setActor] = useState("");
  const [action, setAction] = useState("");
  const { page, loading, pager } = useCursorPages<Schemas["AuditPage"]>("/api/v1/orgs/{org}/audit", interval(range));
  const all = page?.entries ?? [];
  const entries = all.filter((e) => has(e.actor_login, actor) && has(`${e.method} ${e.route} ${e.path}`, action));
  return (
    <List title="Журнал аудита" breadcrumb={false} headerButtons={() => null}>
      <Form layout="inline" style={{ marginBottom: 8, rowGap: 8 }}>
        <Form.Item label="Интервал">
          <DatePicker.RangePicker showTime allowEmpty={[true, true]} value={range} onChange={setRange} />
        </Form.Item>
        <Form.Item label="Администратор" htmlFor="audit-actor">
          <Input id="audit-actor" allowClear placeholder="имя входа" value={actor} onChange={(e) => setActor(e.target.value)} />
        </Form.Item>
        <Form.Item label="Действие" htmlFor="audit-action">
          <Input id="audit-action" allowClear placeholder="метод или путь" value={action} onChange={(e) => setAction(e.target.value)} />
        </Form.Item>
      </Form>
      <Typography.Paragraph type="secondary">
        Администратор и действие отбираются среди записей показанной страницы: показано {entries.length} из {all.length}.
      </Typography.Paragraph>
      <Table<Entry> dataSource={entries} loading={loading} rowKey="seq" pagination={false}>
        <Table.Column dataIndex="seq" title="№" />
        <Table.Column<Entry> title="Время" render={(_, e) => moment(e.at)} />
        <Table.Column dataIndex="actor_login" title="Администратор" />
        <Table.Column<Entry> title="Роли" render={(_, e) => (e.roles.length ? e.roles.map((r) => ROLES[r] ?? r).join(", ") : "нет (отказ)")} />
        <Table.Column<Entry> title="Действие" render={(_, e) => `${e.method} ${e.route}`} />
        <Table.Column dataIndex="path" title="Путь" />
        <Table.Column<Entry> title="Код" render={(_, e) => <Tag color={e.status < 400 ? "green" : "red"}>{e.status}</Tag>} />
      </Table>
      {pager}
    </List>
  );
}
