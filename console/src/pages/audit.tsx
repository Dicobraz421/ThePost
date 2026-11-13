// Журнал аудита (SRS 5.2): операции администраторов. Отбор по интервалу, администратору, методу и началу маршрута —
// в хранилище (ADM-30); продолжение — `next` ответа.
import { List } from "@refinedev/antd";
import { Button, DatePicker, Form, Input, Select, Table, Tag } from "antd";
import type { Dayjs } from "dayjs";
import { useState } from "react";

import type { Schemas } from "../api/client";
import { useCursorPages } from "../components/cursor-pages";
import { interval, moment } from "../components/format";

type Entry = Schemas["AuditEntry"];
type Filters = { actor?: string; method?: string; route?: string; range?: [Dayjs | null, Dayjs | null] | null };

// В записи журнала роли — строки (роль, которой уже нет, показывается как есть); подписи — у всех ролей схемы.
const ROLES: Partial<Record<string, string>> = {
  installation_admin: "администратор инсталляции",
  organization_admin: "администратор организации",
  operator: "оператор",
  auditor: "аудитор",
} satisfies Record<Schemas["AdminRole"], string>;
const METHODS = ["PUT", "POST", "PATCH", "DELETE"].map((m) => ({ value: m, label: m }));

export function AuditPage() {
  const [filters, setFilters] = useState<Filters>({});
  const { page, loading, pager } = useCursorPages<Schemas["AuditPage"]>("/api/v1/orgs/{org}/audit", {
    actor: filters.actor?.trim(),
    method: filters.method,
    route: filters.route?.trim(),
    ...interval(filters.range),
  });
  return (
    <List title="Журнал аудита" breadcrumb={false} headerButtons={() => null}>
      <Form<Filters> layout="inline" onFinish={setFilters} style={{ marginBottom: 16, rowGap: 8 }}>
        <Form.Item name="actor" label="Администратор">
          <Input allowClear placeholder="имя входа или UUID" />
        </Form.Item>
        <Form.Item name="method" label="Метод">
          <Select allowClear options={METHODS} style={{ width: 140 }} />
        </Form.Item>
        <Form.Item name="route" label="Маршрут">
          <Input allowClear placeholder="/api/v1/orgs/{org}/transport" style={{ width: 300 }} />
        </Form.Item>
        <Form.Item name="range" label="Интервал">
          <DatePicker.RangePicker showTime allowEmpty={[true, true]} />
        </Form.Item>
        <Button type="primary" htmlType="submit">
          Найти
        </Button>
      </Form>
      <Table<Entry> dataSource={page?.entries} loading={loading} rowKey="seq" pagination={false}>
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
