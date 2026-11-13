// Политика аудита доступа к ящикам (SRS 4.7, ADM-10): действия не владельца и выбранные действия владельца.
import { Button, Checkbox, Form, Popconfirm, Space, Switch } from "antd";

import type { Schemas } from "../api/client";

type Policy = Schemas["AuditPolicyBody"];

/** Действия владельца, которые узел умеет записывать (`AuditPolicyBody.owner`). */
export const OWNER_ACTIONS: Record<string, string> = {
  delete: "удаление писем",
  move: "перемещение писем",
  send: "отправка",
  change_rules: "изменение правил",
  change_forwarding: "изменение пересылки",
};

export function AuditPolicyForm(p: {
  policy?: Policy;
  readOnly: boolean;
  saving: boolean;
  onSave: (v: Policy) => void;
  /** Сброс: действует политика уровнем выше. */
  reset: { label: string; confirm: string; run: () => void };
}) {
  const initial = p.policy ?? { non_owner: true, owner: [] };
  return (
    <Form<Policy> key={JSON.stringify(initial)} layout="vertical" initialValues={initial} disabled={p.readOnly} onFinish={p.onSave}>
      <Form.Item name="non_owner" label="Записывать действия не владельца" valuePropName="checked" extra="Представители и администраторы.">
        <Switch />
      </Form.Item>
      <Form.Item name="owner" label="Записывать действия владельца">
        <Checkbox.Group options={Object.entries(OWNER_ACTIONS).map(([value, label]) => ({ value, label }))} />
      </Form.Item>
      {!p.readOnly && (
        <Space>
          <Button type="primary" htmlType="submit" loading={p.saving}>
            Сохранить политику аудита
          </Button>
          <Popconfirm title={p.reset.confirm} okText="Сбросить" onConfirm={p.reset.run}>
            <Button>{p.reset.label}</Button>
          </Popconfirm>
        </Space>
      )}
    </Form>
  );
}
