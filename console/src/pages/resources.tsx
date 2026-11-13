// Ресурсы — переговорные и оборудование (UI-10, ADM-16): учётная запись вида resource и её правила бронирования.
// Имя и блокировку входа ведёт карточка учётной записи; представители ресурса — ответственные за бронирования.
import { Create, List, Show, ShowButton, useForm } from "@refinedev/antd";
import { type HttpError, useCustomMutation, useDelete, useList, useShow } from "@refinedev/core";
import { Button, Descriptions, Divider, Form, Input, Popconfirm, Space, Table } from "antd";
import { Link, useNavigate } from "react-router";

import type { Schemas } from "../api/client";
import { type BookingValues, BookingPolicyFields, bookingSummary, fromBooking, toBooking } from "../components/booking-policy-form";
import { Delegates } from "../components/delegates";
import { useReadOnly } from "../providers/access";

type Resource = Schemas["ResourceInfo"];

/** Ресурсы организации. */
export function ResourceList() {
  const readOnly = useReadOnly("resources");
  const { result, query } = useList<Resource>({ resource: "resources", pagination: { mode: "off" } });
  const { mutate: remove } = useDelete();
  return (
    <List title="Ресурсы">
      <Table<Resource> dataSource={result.data} loading={query.isLoading} rowKey="id" pagination={false}>
        <Table.Column dataIndex="display_name" title="Название" />
        <Table.Column dataIndex="login" title="Адрес" />
        <Table.Column<Resource> title="Правила бронирования" render={(_, r) => bookingSummary(r.booking_policy)} />
        <Table.Column<Resource>
          title="Действия"
          width={240}
          render={(_, r) => (
            <Space>
              <ShowButton size="small" recordItemId={r.id} />
              {!readOnly && (
                <Popconfirm
                  title={`Удалить ресурс ${r.display_name}?`}
                  description="Учётная запись ресурса отключается и удаляется обслуживанием; до удаления её можно восстановить в отключённых ящиках."
                  okText="Удалить"
                  onConfirm={() => remove({ resource: "resources", id: r.id, successNotification: { type: "success", message: "Ресурс удалён" } })}
                >
                  <Button size="small" danger>
                    Удалить
                  </Button>
                </Popconfirm>
              )}
            </Space>
          )}
        />
      </Table>
    </List>
  );
}

type NewValues = BookingValues & { login: string; display_name?: string };

/** Новый ресурс: адрес в принятом домене организации, название и правила бронирования. */
export function ResourceCreate() {
  // Поля формы — плоские (правила бронирования — `BookingValues`); тело запроса — `NewResource`.
  const { formProps, saveButtonProps } = useForm<Resource, HttpError, NewValues>({ redirect: "show" });
  return (
    <Create title="Новый ресурс" saveButtonProps={saveButtonProps}>
      <Form
        {...formProps}
        layout="vertical"
        initialValues={fromBooking()}
        onFinish={({ login, display_name, ...booking }) => {
          const body: Schemas["NewResource"] = { login, display_name: display_name?.trim() || null, booking_policy: toBooking(booking) };
          return formProps.onFinish?.(body as unknown as NewValues);
        }}
      >
        <Form.Item
          name="login"
          label="Адрес"
          extra="В принятом домене организации; приводится к нижнему регистру."
          rules={[{ required: true, message: "Укажите адрес" }]}
        >
          <Input placeholder="room-512@example.org" />
        </Form.Item>
        <Form.Item name="display_name" label="Название" extra="Не задано — адрес.">
          <Input placeholder="Переговорная 5.12" />
        </Form.Item>
        <Divider orientation="left">Правила бронирования</Divider>
        <BookingPolicyFields />
      </Form>
    </Create>
  );
}

/** Правила бронирования ресурса: замена целиком. */
function BookingForm({ resource, onSaved }: { resource: Resource; onSaved: () => void }) {
  const readOnly = useReadOnly("resources");
  const { mutate, mutation } = useCustomMutation<Schemas["BookingPolicyBody"], HttpError, Schemas["BookingPolicyBody"]>();
  const initial = fromBooking(resource.booking_policy ?? undefined);
  return (
    <Form<BookingValues>
      key={JSON.stringify(initial)}
      layout="vertical"
      initialValues={initial}
      disabled={readOnly}
      onFinish={(v) =>
        mutate(
          {
            url: `/api/v1/orgs/{org}/resources/${encodeURIComponent(resource.id)}/booking-policy`,
            method: "put",
            values: toBooking(v),
            successNotification: { type: "success", message: "Правила бронирования сохранены" },
          },
          { onSuccess: onSaved },
        )
      }
    >
      <BookingPolicyFields />
      {!readOnly && (
        <Button type="primary" htmlType="submit" loading={mutation.isPending}>
          Сохранить правила
        </Button>
      )}
    </Form>
  );
}

/** Ресурс: сведения, правила бронирования, представители. */
export function ResourceShow() {
  const { result: resource, query } = useShow<Resource>();
  const navigate = useNavigate();
  const readOnly = useReadOnly("resources");
  const { mutate: remove, mutation: removing } = useDelete();
  return (
    <Show
      title={resource?.display_name ?? "Ресурс"}
      isLoading={query.isLoading}
      headerButtons={() =>
        resource &&
        !readOnly && (
          <Popconfirm
            title={`Удалить ресурс ${resource.display_name}?`}
            okText="Удалить"
            onConfirm={() =>
              remove(
                { resource: "resources", id: resource.id, successNotification: { type: "success", message: "Ресурс удалён" } },
                { onSuccess: () => void navigate("/resources") },
              )
            }
          >
            <Button danger loading={removing.isPending}>
              Удалить
            </Button>
          </Popconfirm>
        )
      }
    >
      <Descriptions column={1}>
        <Descriptions.Item label="Адрес">{resource?.login}</Descriptions.Item>
        <Descriptions.Item label="Название">{resource?.display_name}</Descriptions.Item>
        <Descriptions.Item label="Идентификатор">{resource?.id}</Descriptions.Item>
      </Descriptions>
      {resource && <Link to={`/accounts/${encodeURIComponent(resource.id)}`}>Название и блокировка — карточка учётной записи</Link>}
      <Divider orientation="left">Правила бронирования</Divider>
      {resource && <BookingForm resource={resource} onSaved={() => void query.refetch()} />}
      <Divider orientation="left">Представители (ответственные за бронирования)</Divider>
      {resource && <Delegates account={resource.id} resource="resources" />}
    </Show>
  );
}
