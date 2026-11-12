import { Create, DeleteButton, List, Show, ShowButton, useForm, useTable } from "@refinedev/antd";
import { useCustomMutation, useShow } from "@refinedev/core";
import { Descriptions, Divider, Form, Input, Space, Table } from "antd";

import type { Schemas } from "../api/client";
import { PolicyForm } from "../components/policy-form";

type Domain = Schemas["DomainInfo"];

/** Домены организации: список, удаление. */
export function DomainList() {
  const { tableProps } = useTable<Domain>({ pagination: { mode: "off" }, syncWithLocation: false });
  return (
    <List>
      <Table {...tableProps} rowKey="id" pagination={false}>
        <Table.Column dataIndex="name" title="Имя" />
        <Table.Column<Domain>
          title="Действия"
          width={260}
          render={(_, d) => (
            <Space>
              <ShowButton size="small" recordItemId={d.id} />
              <DeleteButton size="small" recordItemId={d.id} confirmTitle={`Удалить домен ${d.name}?`} />
            </Space>
          )}
        />
      </Table>
    </List>
  );
}

/** Новый домен. */
export function DomainCreate() {
  const { formProps, saveButtonProps } = useForm<Domain, never, Schemas["NewDomain"]>({ redirect: "list" });
  return (
    <Create saveButtonProps={saveButtonProps}>
      <Form {...formProps} layout="vertical">
        <Form.Item
          name="name"
          label="Имя домена"
          extra="Латиница или Punycode (A-label); приводится к нижнему регистру."
          rules={[{ required: true, message: "Укажите имя домена" }]}
        >
          <Input placeholder="example.org" />
        </Form.Item>
      </Form>
    </Create>
  );
}

/** Домен и его политика. */
export function DomainShow() {
  const { result: domain, query } = useShow<Schemas["DomainDetails"]>();
  const { mutate, mutation } = useCustomMutation<Schemas["PolicyBody"]>();
  return (
    <Show title={domain?.name ?? "Домен"} isLoading={query.isLoading}>
      <Descriptions column={1}>
        <Descriptions.Item label="Имя">{domain?.name}</Descriptions.Item>
        <Descriptions.Item label="Идентификатор">{domain?.id}</Descriptions.Item>
      </Descriptions>
      <Divider orientation="left">Политика домена</Divider>
      <PolicyForm
        policies={domain}
        saving={mutation.isPending}
        onSave={(values) =>
          domain &&
          mutate(
            {
              url: `/api/v1/orgs/{org}/domains/${encodeURIComponent(domain.id)}/policy`,
              method: "put",
              values,
              successNotification: { type: "success", message: "Политика домена сохранена" },
            },
            { onSuccess: () => void query.refetch() },
          )
        }
      />
    </Show>
  );
}
