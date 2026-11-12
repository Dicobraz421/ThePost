// Редактор политики (UI-03): поля формы строятся по схеме `PolicyBody` из OpenAPI узла — подписи, типы,
// перечисления и вложенные правила берутся из схемы, новое поле политики появляется в форме без правки консоли.
import { Button, Card, Form, Input, InputNumber, Select, Switch } from "antd";

import type { Schemas } from "../api/client";
import spec from "../api/openapi.json";

type Schema = {
  type?: string | string[];
  $ref?: string;
  oneOf?: Schema[];
  properties?: Record<string, Schema>;
  required?: string[];
  enum?: string[];
  description?: string;
  example?: unknown;
  minimum?: number;
};

const schemas = spec.components.schemas as Record<string, Schema>;
const types = (s: Schema) => [s.type ?? []].flat();
const nullable = (s: Schema) => types(s).includes("null") || !!s.oneOf?.some(nullable);

/** Раскрывает `$ref` и «значение или null»; описание места использования важнее описания типа. */
function resolve(s: Schema): Schema {
  const target = s.$ref ? schemas[s.$ref.split("/").pop() ?? ""] : s.oneOf?.find((v) => !types(v).includes("null"));
  if (!target) return s;
  const r = resolve(target);
  return { ...r, description: s.description ?? r.description };
}

/** Значения формы → тело запроса: пустое — `null`; вложенное правило без обязательного поля не задано. */
export function toBody(schema: Schema, value: unknown): unknown {
  const s = resolve(schema);
  if (!s.properties) return value === undefined || value === "" ? null : value;
  const fields = Object.entries(s.properties).map(([k, sub]) => [k, toBody(sub, (value as Record<string, unknown>)?.[k]), sub] as const);
  if (fields.some(([k, v]) => v === null && s.required?.includes(k))) return null;
  return Object.fromEntries(fields.filter(([, v, sub]) => v !== null || nullable(sub)).map(([k, v]) => [k, v]));
}

function Field({ path, schema, effective }: { path: string[]; schema: Schema; effective?: unknown }) {
  const s = resolve(schema);
  if (s.properties) {
    return (
      <Card size="small" title={s.description} style={{ marginBottom: 24 }}>
        {Object.entries(s.properties).map(([k, sub]) => (
          <Field key={k} path={[...path, k]} schema={sub} effective={(effective as Record<string, unknown>)?.[k]} />
        ))}
      </Card>
    );
  }
  const type = types(s).find((t) => t !== "null");
  const control = s.enum ? (
    <Select allowClear options={s.enum.map((value) => ({ value }))} />
  ) : type === "integer" ? (
    <InputNumber min={s.minimum} style={{ width: "100%" }} />
  ) : type === "boolean" ? (
    <Switch />
  ) : (
    <Input placeholder={s.example === undefined ? undefined : String(s.example)} />
  );
  return (
    <Form.Item
      name={path}
      label={s.description ?? path.join(".")}
      valuePropName={type === "boolean" ? "checked" : "value"}
      extra={effective === undefined || effective === null ? undefined : `Действует: ${String(effective)}`}
    >
      {control}
    </Form.Item>
  );
}

const root: Schema = { $ref: "#/components/schemas/PolicyBody" };

/** Политика уровня (организация, домен): заданное на уровне — в полях, действующее — в подсказках. */
export function PolicyForm(p: {
  policies?: Schemas["Policies"];
  saving?: boolean;
  onSave: (body: Schemas["PolicyBody"]) => void;
}) {
  return (
    <Form
      key={JSON.stringify(p.policies?.policy)}
      layout="vertical"
      initialValues={p.policies?.policy}
      onFinish={(v) => p.onSave(toBody(root, v) as Schemas["PolicyBody"])}
    >
      {Object.entries(resolve(root).properties ?? {}).map(([k, sub]) => (
        <Field key={k} path={[k]} schema={sub} effective={(p.policies?.effective as Record<string, unknown>)?.[k]} />
      ))}
      <Button type="primary" htmlType="submit" loading={p.saving}>
        Сохранить политику
      </Button>
    </Form>
  );
}
