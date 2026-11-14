// Очередь исходящей почты организации (ADM-45): записи с получателями и попытками, фильтры, повтор доставки.
import { List } from "@refinedev/antd";
import { useCustomMutation, useInfiniteList } from "@refinedev/core";
import { Button, Input, Popconfirm, Select, Space, Table, Tag, Typography } from "antd";
import { useState } from "react";

import type { Schemas } from "../api/client";
import { bytes, when } from "../components/format";
import { useReadOnly } from "../providers/access";

type Entry = Schemas["QueueEntry"];
type Rcpt = Schemas["QueueRcptInfo"];
type Status = Schemas["QueueStatus"];

const STATUS: Record<Status, { label: string; color: string }> = {
  ready: { label: "Ждёт попытки", color: "blue" },
  retry: { label: "Повтор", color: "orange" },
  active: { label: "Доставляется", color: "green" },
};

const RCPT: Record<Rcpt["status"], string> = { pending: "ждёт", delivered: "доставлено", failed: "отказ" };

const rcpt = (r: Rcpt) => (
  <div key={r.address}>
    {`${r.address} — ${RCPT[r.status]}${r.attempts > 0 ? `, попыток: ${r.attempts}` : ""}`}
    {r.last_error && <Typography.Text type="secondary">{` (${r.last_error})`}</Typography.Text>}
  </div>
);

/** Записи очереди: фильтр по состоянию и адресу, страницы по `Link`; «Повторить» — попытка сейчас. */
export function QueuePage() {
  const [status, setStatus] = useState<Status>();
  const [address, setAddress] = useState("");
  const readOnly = useReadOnly("queue");
  const { query } = useInfiniteList<Entry>({
    resource: "queue",
    pagination: { mode: "off" },
    filters: [
      ...(status ? [{ field: "status", operator: "eq" as const, value: status }] : []),
      ...(address ? [{ field: "address", operator: "eq" as const, value: address }] : []),
    ],
    queryOptions: { getNextPageParam: (last) => last.cursor?.next },
  });
  const { mutate, mutation } = useCustomMutation<never, never, undefined>();
  const retry = (e: Entry) =>
    mutate(
      {
        url: `/api/v1/orgs/{org}/queue/${encodeURIComponent(e.id)}/retry`,
        method: "post",
        values: undefined,
        successNotification: { type: "success", message: "Повтор доставки запрошен" },
      },
      { onSuccess: () => void query.refetch() },
    );
  return (
    <List title="Очередь исходящей почты">
      <Space wrap style={{ marginBottom: 16 }}>
        <Select<Status>
          aria-label="Состояние"
          placeholder="Все состояния"
          allowClear
          style={{ width: 200 }}
          value={status}
          onChange={setStatus}
          options={Object.entries(STATUS).map(([value, s]) => ({ value, label: s.label }))}
        />
        <Input.Search
          aria-label="Адрес"
          placeholder="Отправитель или получатель"
          allowClear
          enterButton="Найти"
          onSearch={(v) => setAddress(v.trim())}
          style={{ width: 360 }}
        />
      </Space>
      <Table<Entry> dataSource={query.data?.pages.flatMap((p) => p.data)} loading={query.isLoading} rowKey="id" pagination={false}>
        <Table.Column<Entry> title="Отправитель" render={(_, e) => e.from || "уведомление о недоставке"} />
        <Table.Column<Entry> title="Получатели" render={(_, e) => e.rcpts.map(rcpt)} />
        <Table.Column<Entry>
          title="Состояние"
          render={(_, e) => <Tag color={STATUS[e.status].color}>{STATUS[e.status].label}</Tag>}
        />
        <Table.Column<Entry> title="Ближайшая попытка" render={(_, e) => when(e.next_attempt)} />
        <Table.Column<Entry> title="В очереди с" render={(_, e) => when(e.queued_at)} />
        <Table.Column<Entry> title="Размер" render={(_, e) => bytes(e.size)} />
        {!readOnly && (
          <Table.Column<Entry>
            title="Действия"
            width={140}
            render={(_, e) => (
              <Popconfirm
                title="Повторить доставку сейчас?"
                okText="Повторить"
                cancelText="Отмена"
                disabled={e.status === "active"}
                onConfirm={() => retry(e)}
              >
                <Button size="small" disabled={e.status === "active"} loading={mutation.isPending}>
                  Повторить
                </Button>
              </Popconfirm>
            )}
          />
        )}
      </Table>
      {query.hasNextPage && (
        <Button onClick={() => void query.fetchNextPage()} loading={query.isFetchingNextPage} style={{ marginTop: 16 }}>
          Показать ещё
        </Button>
      )}
    </List>
  );
}
