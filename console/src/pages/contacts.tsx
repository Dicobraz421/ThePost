// Адресная книга: почтовые контакты организации (внешние адреса в GAL и группах рассылки; ADM-10).
import { Create, DeleteButton, Edit, EditButton, List, useForm, useTable } from "@refinedev/antd";
import { type HttpError, useList, useShow, useUpdate } from "@refinedev/core";
import { Alert, Button, Form, Input, Select, Space, Switch, Table, Tag } from "antd";

import type { Schemas } from "../api/client";

type Contact = Schemas["ContactInfo"];
/** Значения формы: атрибуты — список пар (порядок и пустые строки — в форме, не в теле). */
type ContactForm = Omit<Schemas["ContactBody"], "attributes"> & { attributes: { name?: string; value?: string }[] };

export function toForm(c?: Contact): ContactForm {
  return {
    display_name: c?.display_name ?? "",
    email: c?.email ?? "",
    groups: c?.groups ?? [],
    hidden_from_gal: c?.hidden_from_gal ?? false,
    attributes: Object.entries(c?.attributes ?? {}).map(([name, value]) => ({ name, value })),
  };
}

/** Тело `POST`/`PUT` (замена целиком); атрибут без имени не отправляется. */
export function toBody(v: ContactForm): Schemas["ContactBody"] {
  return {
    display_name: v.display_name.trim(),
    email: v.email.trim(),
    groups: v.groups ?? [],
    hidden_from_gal: v.hidden_from_gal ?? false,
    attributes: Object.fromEntries(
      (v.attributes ?? []).flatMap((a) => (a?.name?.trim() ? [[a.name.trim(), a.value ?? ""]] : [])),
    ),
  };
}

function useGroupNames() {
  const { result } = useList<Schemas["GroupInfo"]>({ resource: "groups", pagination: { mode: "off" } });
  return result.data;
}

/** Контакты организации. */
export function ContactList() {
  const { tableProps } = useTable<Contact>({ pagination: { mode: "off" }, syncWithLocation: false });
  const groups = useGroupNames();
  const name = (id: string) => groups.find((g) => g.id === id)?.name ?? id;
  return (
    <List title="Адресная книга">
      <Table {...tableProps} rowKey="id" pagination={false}>
        <Table.Column dataIndex="display_name" title="Имя" />
        <Table.Column dataIndex="email" title="Адрес" />
        <Table.Column<Contact> title="Группы рассылки" render={(_, c) => c.groups.map(name).join(", ")} />
        <Table.Column<Contact>
          title="Свойства"
          render={(_, c) => (
            <>
              {c.external_id && <Tag>каталог</Tag>}
              {c.hidden_from_gal && <Tag>скрыт из адресной книги</Tag>}
            </>
          )}
        />
        <Table.Column<Contact>
          title="Действия"
          width={260}
          render={(_, c) => (
            <Space>
              <EditButton size="small" recordItemId={c.id} />
              <DeleteButton size="small" recordItemId={c.id} disabled={!!c.external_id} confirmTitle={`Удалить контакт ${c.email}?`} />
            </Space>
          )}
        />
      </Table>
    </List>
  );
}

function ContactFields() {
  const groups = useGroupNames();
  return (
    <>
      <Form.Item name="display_name" label="Отображаемое имя" rules={[{ required: true, whitespace: true, message: "Укажите имя" }]}>
        <Input maxLength={255} />
      </Form.Item>
      <Form.Item name="email" label="Адрес" extra="Внешний адрес; в организации — один контакт на адрес." rules={[{ required: true, message: "Укажите адрес" }]}>
        <Input placeholder="partner@example.com" />
      </Form.Item>
      <Form.Item name="groups" label="Группы рассылки">
        <Select mode="multiple" options={groups.map((g) => ({ value: g.id, label: g.name }))} optionFilterProp="label" />
      </Form.Item>
      <Form.Item name="hidden_from_gal" label="Скрыть из адресной книги" valuePropName="checked">
        <Switch />
      </Form.Item>
      <Form.Item label="Атрибуты" extra="Для адресной книги и фильтров динамических групп; имя — латиницей.">
        <Form.List name="attributes">
          {(fields, { add, remove }) => (
            <>
              {fields.map((f) => (
                <Space key={f.key} align="baseline">
                  <Form.Item name={[f.name, "name"]} rules={[{ pattern: /^[\x21-\x7e]+$/, message: "Имя — ASCII без пробелов" }]}>
                    <Input aria-label="Имя атрибута" placeholder="department" />
                  </Form.Item>
                  <Form.Item name={[f.name, "value"]}>
                    <Input aria-label="Значение атрибута" placeholder="Продажи" />
                  </Form.Item>
                  <Button onClick={() => remove(f.name)}>Убрать</Button>
                </Space>
              ))}
              <Button onClick={() => add({ name: "", value: "" })}>Добавить атрибут</Button>
            </>
          )}
        </Form.List>
      </Form.Item>
    </>
  );
}

/** Новый контакт (не из каталога). */
export function ContactCreate() {
  const { formProps, saveButtonProps } = useForm<Contact, HttpError, ContactForm>({ redirect: "list" });
  return (
    <Create saveButtonProps={saveButtonProps} title="Новый контакт">
      <Form
        {...formProps}
        layout="vertical"
        initialValues={toForm()}
        onFinish={(v) => formProps.onFinish?.(toBody(v) as unknown as ContactForm)}
      >
        <ContactFields />
      </Form>
    </Create>
  );
}

/** Контакт: замена целиком (`PUT`); контакт каталога ведёт каталог — только просмотр. */
export function ContactEdit() {
  const { result: contact, query } = useShow<Contact>();
  const { mutate, mutation } = useUpdate<Contact>();
  const [form] = Form.useForm<ContactForm>();
  const catalog = !!contact?.external_id;
  return (
    <Edit
      title={contact?.display_name ?? "Контакт"}
      isLoading={query.isLoading}
      saveButtonProps={{ onClick: () => form.submit(), loading: mutation.isPending, disabled: catalog }}
      deleteButtonProps={{ disabled: catalog, confirmTitle: `Удалить контакт ${contact?.email ?? ""}?` }}
    >
      {catalog && <Alert type="info" showIcon style={{ marginBottom: 16 }} message="Контакт каталога: его ведёт каталог." />}
      {contact && (
        <Form
          key={JSON.stringify(contact)}
          form={form}
          layout="vertical"
          disabled={catalog}
          initialValues={toForm(contact)}
          onFinish={(v) =>
            mutate({
              resource: "contacts",
              id: contact.id,
              values: toBody(v),
              successNotification: { type: "success", message: "Контакт сохранён" },
            })
          }
        >
          <ContactFields />
        </Form>
      )}
    </Edit>
  );
}
