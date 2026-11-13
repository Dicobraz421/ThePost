// Политика общего доступа организации к календарям и адресным книгам (ADM-16, APP-I): кому можно выдавать права и
// наибольшие права вне групп политики и «всем». Ужесточение уже выданное не удаляет — узел ограничивает его права.
import { type HttpError, useCustom, useCustomMutation } from "@refinedev/core";
import { Button, Form, Radio } from "antd";

import type { Schemas } from "../api/client";
import { useReadOnly } from "../providers/access";
import { GroupSelect } from "./group-select";
import { type Right, RightsChecks, fromRights, toRights } from "./rights";

type Policy = Schemas["SharingPolicyBody"];
type Values = { scope: Policy["scope"]["type"]; groups: string[]; max_outside: Right[] };

const POLICY_URL = "/api/v1/orgs/{org}/calendar-sharing";

const SCOPES: Record<Values["scope"], string> = {
  organization: "любому субъекту организации",
  groups: "полные права — участникам групп, остальным — ограниченные",
  nobody: "общий доступ запрещён",
};

const toValues = (p: Policy): Values => ({
  scope: p.scope.type,
  groups: p.scope.type === "groups" ? p.scope.groups : [],
  max_outside: fromRights(p.max_outside),
});

const toPolicy = (v: Values): Policy => ({
  scope: v.scope === "groups" ? { type: "groups", groups: v.groups ?? [] } : { type: v.scope },
  max_outside: toRights(v.max_outside),
});

export function SharingPolicyForm() {
  const readOnly = useReadOnly("organization");
  const { result, query } = useCustom<Policy>({ url: POLICY_URL, method: "get" });
  const { mutate, mutation } = useCustomMutation<Policy, HttpError, Policy>();
  const [form] = Form.useForm<Values>();
  const scope = Form.useWatch("scope", form);
  if (query.isLoading || !result.data) return null;
  const initial = toValues(result.data);
  return (
    <Form<Values>
      key={JSON.stringify(initial)}
      form={form}
      layout="vertical"
      initialValues={initial}
      disabled={readOnly}
      onFinish={(v) =>
        mutate(
          { url: POLICY_URL, method: "put", values: toPolicy(v), successNotification: { type: "success", message: "Политика общего доступа сохранена" } },
          { onSuccess: () => void query.refetch() },
        )
      }
    >
      <Form.Item name="scope" label="Кому можно выдавать права на календари и адресные книги">
        <Radio.Group options={Object.entries(SCOPES).map(([value, label]) => ({ value, label }))} />
      </Form.Item>
      {scope === "groups" && (
        <Form.Item name="groups" label="Группы с полными правами" rules={[{ required: true, message: "Выберите группы" }]}>
          <GroupSelect />
        </Form.Item>
      )}
      {scope !== "nobody" && (
        <Form.Item
          name="max_outside"
          label={scope === "groups" ? "Наибольшие права остальным и «всем»" : "Наибольшие права «всем» (anyone)"}
        >
          <RightsChecks />
        </Form.Item>
      )}
      {!readOnly && (
        <Button type="primary" htmlType="submit" loading={mutation.isPending}>
          Сохранить политику общего доступа
        </Button>
      )}
    </Form>
  );
}
