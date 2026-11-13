// Представители учётной записи (ADM-16): читают или ведут все её календари и могут отправлять приглашения и ответы
// от её имени; у ресурса — ответственные за бронирования. Назначение сверх политики общего доступа узел отклоняет.
import { type HttpError, useCustom, useCustomMutation } from "@refinedev/core";
import { Button, Form, Popconfirm, Select, Switch, Table, Tag } from "antd";

import type { Schemas } from "../api/client";
import { useReadOnly } from "../providers/access";
import { AccountPick } from "./account-select";

type Delegate = Schemas["DelegateInfo"];
type Values = Schemas["DelegateBody"] & { delegate: string };

/** Список ответа; до ответа и при ошибке узла — пусто. */
const list = <T,>(data: unknown): T[] => (Array.isArray(data) ? (data as T[]) : []);

const LEVELS: Record<Schemas["DelegateLevelBody"], string> = { read: "чтение", write: "чтение и запись" };

export function Delegates({ account, resource = "accounts" }: { account: string; resource?: string }) {
  const readOnly = useReadOnly(resource);
  const url = `/api/v1/orgs/{org}/accounts/${encodeURIComponent(account)}/delegates`;
  const { result, query } = useCustom<Delegate[]>({ url, method: "get" });
  const { mutate, mutation } = useCustomMutation<Delegate, HttpError, Schemas["DelegateBody"] | undefined>();
  const [form] = Form.useForm<Values>();
  const done = () => {
    form.resetFields();
    void query.refetch();
  };
  const save = ({ delegate, level, schedule_on_behalf }: Values, message: string) =>
    mutate(
      {
        url: `${url}/${encodeURIComponent(delegate)}`,
        method: "put",
        values: { level, schedule_on_behalf },
        successNotification: { type: "success", message },
      },
      { onSuccess: done },
    );
  return (
    <>
      {!readOnly && (
        <Form<Values>
          form={form}
          layout="inline"
          initialValues={{ level: "read", schedule_on_behalf: false }}
          style={{ marginBottom: 16 }}
          onFinish={(v) => save(v, "Представитель назначен")}
        >
          <Form.Item name="delegate" label="Представитель" rules={[{ required: true, message: "Выберите учётную запись" }]}>
            <AccountPick />
          </Form.Item>
          <Form.Item name="level" label="Доступ к календарям">
            <Select style={{ width: 170 }} options={Object.entries(LEVELS).map(([value, label]) => ({ value, label }))} />
          </Form.Item>
          <Form.Item name="schedule_on_behalf" label="Приглашения от имени" valuePropName="checked">
            <Switch />
          </Form.Item>
          <Button type="primary" htmlType="submit" loading={mutation.isPending}>
            Назначить
          </Button>
        </Form>
      )}
      <Table<Delegate> dataSource={list(result.data)} loading={query.isLoading} rowKey="delegate" pagination={false} size="small">
        <Table.Column dataIndex="login" title="Имя входа" />
        <Table.Column dataIndex="display_name" title="Отображаемое имя" />
        <Table.Column<Delegate>
          title="Доступ к календарям"
          render={(_, d) =>
            readOnly ? (
              LEVELS[d.level]
            ) : (
              <Select
                aria-label={`Доступ ${d.login}`}
                size="small"
                value={d.level}
                style={{ width: 170 }}
                options={Object.entries(LEVELS).map(([value, label]) => ({ value, label }))}
                onChange={(level) => save({ ...d, level }, "Доступ представителя изменён")}
              />
            )
          }
        />
        <Table.Column<Delegate>
          title="Приглашения от имени"
          render={(_, d) => (d.schedule_on_behalf ? <Tag color="blue">да</Tag> : "нет")}
        />
        {!readOnly && (
          <Table.Column<Delegate>
            title="Действия"
            width={140}
            render={(_, d) => (
              <Popconfirm
                title={`Отозвать представителя ${d.login}?`}
                okText="Отозвать"
                onConfirm={() =>
                  mutate(
                    {
                      url: `${url}/${encodeURIComponent(d.delegate)}`,
                      method: "delete",
                      values: undefined,
                      successNotification: { type: "success", message: "Представитель отозван" },
                    },
                    { onSuccess: done },
                  )
                }
              >
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
