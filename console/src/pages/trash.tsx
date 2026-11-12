import { Show } from "@refinedev/antd";
import { type HttpError, useCustom, useCustomMutation, useOne } from "@refinedev/core";
import { Button, Descriptions, Popconfirm, Space, Table, Tabs } from "antd";
import { useState } from "react";
import { useParams } from "react-router";

import type { Schemas } from "../api/client";
import { bytes, when } from "../components/format";

type Entry = Schemas["TrashEntry"];

const MODES: Record<Schemas["PurgeMode"], string> = {
  hide: "скрыть до конца срока корзины (hide)",
  erase: "удалить окончательно (erase)",
};

/** Страница корзины (`hidden` — скрытые письма): размер страницы задаёт узел, переход — по `next_offset`. */
function TrashTable({ account, hidden }: { account: string; hidden?: boolean }) {
  const url = `/api/v1/orgs/{org}/accounts/${encodeURIComponent(account)}/trash${hidden ? "/hidden" : ""}`;
  // Смещения пройденных страниц: последнее — текущая.
  const [offsets, setOffsets] = useState([0]);
  const offset = offsets[offsets.length - 1];
  const { result, query } = useCustom<Schemas["TrashPage"]>({ url, method: "get", config: { query: { offset } } });
  const { mutate, mutation } = useCustomMutation<Schemas["RestoredMessage"], HttpError, undefined>();
  const page = result.data;
  const next = page?.next_offset;
  return (
    <>
      <Table<Entry> dataSource={page?.items} loading={query.isLoading} rowKey="message" pagination={false}>
        <Table.Column dataIndex="message" title="Письмо" />
        <Table.Column dataIndex="origin" title="Исходная папка" />
        <Table.Column<Entry> title="Размер" render={(_, e) => bytes(e.size)} />
        <Table.Column<Entry> title="Принято" render={(_, e) => when(e.received_at)} />
        <Table.Column<Entry> title="Удалено" render={(_, e) => when(e.deleted_at)} />
        <Table.Column<Entry> title="Хранится до" render={(_, e) => when(e.expires_at)} />
        <Table.Column<Entry>
          title="Действия"
          width={160}
          render={(_, e) => (
            <Popconfirm
              title={`Восстановить письмо в папку «${e.origin}»?`}
              okText="Восстановить"
              onConfirm={() =>
                mutate(
                  {
                    url: `${url}/${encodeURIComponent(e.message)}/restore`,
                    method: "post",
                    values: undefined,
                    successNotification: { type: "success", message: "Письмо восстановлено" },
                  },
                  { onSuccess: () => void query.refetch() },
                )
              }
            >
              <Button size="small" loading={mutation.isPending}>
                Восстановить
              </Button>
            </Popconfirm>
          )}
        />
      </Table>
      <Space style={{ marginTop: 16 }}>
        <Button disabled={offsets.length < 2} onClick={() => setOffsets(offsets.slice(0, -1))}>
          Назад
        </Button>
        <Button disabled={next == null} onClick={() => next != null && setOffsets([...offsets, next])}>
          Дальше
        </Button>
      </Space>
    </>
  );
}

/** Корзина и скрытые письма учётной записи: срок корзины, правило `trash.purge`, восстановление. */
export function AccountTrash() {
  const { id = "" } = useParams();
  const item = `/api/v1/orgs/{org}/accounts/${encodeURIComponent(id)}`;
  const { result: account } = useOne<Schemas["AccountInfo"]>({ resource: "accounts", id });
  const { result: purge } = useCustom<Schemas["TrashPurgeInfo"]>({ url: `${item}/trash-purge`, method: "get" });
  const rule = purge.data?.rule;
  const retention = account?.effective.trash_retention;
  return (
    <Show title={`Корзина: ${account?.login ?? ""}`} breadcrumb={false} headerButtons={() => null}>
      <Descriptions column={1} title="Очистка корзины пользователем (trash.purge)">
        <Descriptions.Item label="Срок корзины">{retention ?? "по умолчанию (конфигурация узла)"}</Descriptions.Item>
        <Descriptions.Item label="Правило администратора">{rule && MODES[rule.mode]}</Descriptions.Item>
        <Descriptions.Item label="Пользователь может выбрать своё значение">
          {rule && (rule.user_can_choose ? "да" : "нет")}
        </Descriptions.Item>
        <Descriptions.Item label="Выбор пользователя">{purge.data?.user ? MODES[purge.data.user] : "не выбран"}</Descriptions.Item>
        <Descriptions.Item label="Действует">{purge.data && MODES[purge.data.effective]}</Descriptions.Item>
      </Descriptions>
      <Tabs
        destroyOnHidden
        items={[
          { key: "trash", label: "Корзина", children: <TrashTable account={id} /> },
          {
            key: "hidden",
            label: "Скрытые письма",
            children: <TrashTable account={id} hidden />,
          },
        ]}
      />
    </Show>
  );
}
