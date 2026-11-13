import { Create, DeleteButton, Edit, EditButton, List, useForm, useTable } from "@refinedev/antd";
import { type HttpError, useList, useShow, useUpdate } from "@refinedev/core";
import { Divider, Form, Input, Select, Space, Switch, Table, Tag } from "antd";

import type { Schemas } from "../api/client";
import { AccountSelect } from "../components/account-select";
import { DynamicFilter } from "../components/dynamic-filter";
import { ModerationQueue } from "../components/moderation-queue";

type Group = Schemas["GroupInfo"];

/** Значения формы группы: участники и отправители разложены по видам. */
type GroupForm = {
  name: string;
  address?: string | null;
  hidden_from_gal: boolean;
  member_accounts: string[];
  member_groups: string[];
  senders: "anyone" | "internal" | "only";
  only_accounts: string[];
  only_groups: string[];
  moderators: string[];
};

export function toForm(g?: Group): GroupForm {
  const s = g?.senders ?? "anyone";
  const only = typeof s === "object" ? s.only : { accounts: [], groups: [] };
  return {
    name: g?.name ?? "",
    address: g?.address,
    hidden_from_gal: g?.hidden_from_gal ?? false,
    member_accounts: g?.members.flatMap((m) => ("account" in m ? [m.account] : [])) ?? [],
    member_groups: g?.members.flatMap((m) => ("group" in m ? [m.group] : [])) ?? [],
    senders: typeof s === "object" ? "only" : s,
    only_accounts: only.accounts,
    only_groups: only.groups,
    moderators: g?.moderators ?? [],
  };
}

/** Тело запроса; у группы каталога имя, адрес и состав ведёт каталог — эти поля не отправляются. */
export function toBody(v: GroupForm, catalog = false) {
  const own = {
    name: v.name,
    address: v.address?.trim() || null,
    members: [...v.member_accounts.map((account) => ({ account })), ...v.member_groups.map((group) => ({ group }))],
  };
  return {
    ...(catalog ? {} : own),
    hidden_from_gal: v.hidden_from_gal,
    senders: v.senders === "only" ? { only: { accounts: v.only_accounts, groups: v.only_groups } } : v.senders,
    moderators: v.moderators,
  };
}

const moderated = (g: Group) => g.moderators.length > 0;

/** Группы рассылки организации. */
export function GroupList() {
  const { tableProps } = useTable<Group>({ pagination: { mode: "off" }, syncWithLocation: false });
  return (
    <List>
      <Table {...tableProps} rowKey="id" pagination={false}>
        <Table.Column dataIndex="name" title="Имя" />
        <Table.Column<Group> title="Адрес" render={(_, g) => g.address ?? "—"} />
        <Table.Column<Group> title="Участников" render={(_, g) => g.members.length} />
        <Table.Column<Group>
          title="Свойства"
          render={(_, g) => (
            <>
              {g.external_id && <Tag>каталог</Tag>}
              {moderated(g) && <Tag color="blue">модерация</Tag>}
              {g.senders !== "anyone" && <Tag color="gold">ограничены отправители</Tag>}
              {g.hidden_from_gal && <Tag>скрыта из адресной книги</Tag>}
            </>
          )}
        />
        <Table.Column<Group>
          title="Действия"
          width={260}
          render={(_, g) => (
            <Space>
              <EditButton size="small" recordItemId={g.id} />
              <DeleteButton
                size="small"
                recordItemId={g.id}
                disabled={!!g.external_id}
                confirmTitle={`Удалить группу ${g.name}? Письма её очереди модерации будут отклонены.`}
              />
            </Space>
          )}
        />
      </Table>
    </List>
  );
}

/**
 * Поля группы; `catalog` — группа из каталога: имя, адрес и состав не редактируются. `names` — имена участников,
 * отправителей и модераторов из ответа узла.
 */
function GroupFields({ catalog, self, names }: { catalog?: boolean; self?: string; names?: Record<string, string> }) {
  const { result: groups } = useList<Group>({ resource: "groups", pagination: { mode: "off" } });
  const options = groups.data.filter((g) => g.id !== self).map((g) => ({ value: g.id, label: g.name }));
  const restricted = Form.useWatch("senders") === "only";
  const fromCatalog = catalog ? "Ведётся в каталоге." : undefined;
  return (
    <>
      <Form.Item name="name" label="Имя группы" extra={fromCatalog} rules={[{ required: true, message: "Укажите имя группы" }]}>
        <Input disabled={catalog} />
      </Form.Item>
      <Form.Item name="address" label="Адрес рассылки" extra={fromCatalog ?? "Не задан — группа без почты (только вложенная)."}>
        <Input disabled={catalog} placeholder="staff@example.org" />
      </Form.Item>
      <Form.Item name="member_accounts" label="Участники — учётные записи" extra={fromCatalog}>
        <AccountSelect disabled={catalog} names={names} />
      </Form.Item>
      <Form.Item name="member_groups" label="Участники — вложенные группы" extra={fromCatalog}>
        <Select mode="multiple" options={options} optionFilterProp="label" disabled={catalog} />
      </Form.Item>
      <Form.Item name="senders" label="Кто может писать в группу">
        <Select
          options={[
            { value: "anyone", label: "Все" },
            { value: "internal", label: "Только отправители организации" },
            { value: "only", label: "Только перечисленные" },
          ]}
        />
      </Form.Item>
      {restricted && (
        <>
          <Form.Item name="only_accounts" label="Разрешённые отправители — учётные записи">
            <AccountSelect names={names} />
          </Form.Item>
          <Form.Item name="only_groups" label="Разрешённые отправители — участники групп">
            <Select mode="multiple" options={groups.data.map((g) => ({ value: g.id, label: g.name }))} optionFilterProp="label" />
          </Form.Item>
        </>
      )}
      <Form.Item
        name="moderators"
        label="Модераторы"
        extra="Заданы — письма не от модераторов ждут одобрения в очереди модерации."
      >
        <AccountSelect names={names} />
      </Form.Item>
      <Form.Item name="hidden_from_gal" label="Скрыть из адресной книги" valuePropName="checked">
        <Switch />
      </Form.Item>
    </>
  );
}

/** Новая группа (не из каталога). */
export function GroupCreate() {
  const { formProps, saveButtonProps } = useForm<Group, HttpError, GroupForm>({ redirect: "list" });
  return (
    <Create saveButtonProps={saveButtonProps}>
      <Form
        {...formProps}
        layout="vertical"
        initialValues={toForm()}
        onFinish={(v) => formProps.onFinish?.(toBody(v) as unknown as GroupForm)}
      >
        <GroupFields />
      </Form>
    </Create>
  );
}

/** Группа: состав (перечнем или фильтром), ограничения отправителей, модераторы и очередь модерации группы. */
export function GroupEdit() {
  const { result: group, query } = useShow<Group>();
  const { mutate, mutation } = useUpdate<Group>();
  const [form] = Form.useForm<GroupForm>();
  const catalog = !!group?.external_id;
  return (
    <Edit
      title={group?.name ?? "Группа"}
      isLoading={query.isLoading}
      saveButtonProps={{ onClick: () => form.submit(), loading: mutation.isPending }}
      deleteButtonProps={{ disabled: catalog }}
    >
      {group && (
        <Form
          key={JSON.stringify(group)}
          form={form}
          layout="vertical"
          initialValues={toForm(group)}
          onFinish={(v) =>
            mutate({
              resource: "groups",
              id: group.id,
              values: toBody(v, catalog),
              meta: { method: "PATCH" },
              successNotification: { type: "success", message: "Группа сохранена" },
            })
          }
        >
          <GroupFields catalog={catalog} self={group.id} names={group.names} />
        </Form>
      )}
      {group && (
        <>
          <Divider orientation="left">Динамический состав</Divider>
          <DynamicFilter group={group.id} members={group.members.length} />
        </>
      )}
      {group && moderated(group) && (
        <>
          <Divider orientation="left">Очередь модерации группы</Divider>
          <ModerationQueue group={group.id} />
        </>
      )}
    </Edit>
  );
}
