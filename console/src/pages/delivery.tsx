// Журнал доставки (SRS 5.2): поиск по отправителю, получателю, Message-ID и интервалу; карточка письма с событиями.
import { List } from "@refinedev/antd";
import { Button, DatePicker, Descriptions, Drawer, Form, Input, Table, Tag, Timeline, Typography } from "antd";
import type { Dayjs } from "dayjs";
import { useState } from "react";

import type { Schemas } from "../api/client";
import { useCursorPages } from "../components/cursor-pages";
import { interval, moment } from "../components/format";

type Entry = Schemas["DeliveryEntry"];
type Filters = { sender?: string; recipient?: string; message_id?: string; range?: [Dayjs | null, Dayjs | null] | null };

const URL = "/api/v1/orgs/{org}/delivery";
const KINDS: Record<string, string> = { receive: "приём", reject: "отказ", deliver: "доставка", relay: "передача наружу", dsn: "DSN" };
const DECISIONS: Record<string, [string, string]> = {
  accept: ["принято", "green"],
  reject: ["отклонено", "red"],
  defer: ["отложено", "orange"],
  quarantine: ["карантин", "purple"],
  discard: ["отброшено", "default"],
  hold: ["задержано", "blue"],
};

const kind = (e: Entry) => KINDS[e.kind] ?? e.kind;
const decision = (e: Entry) => <Tag color={DECISIONS[e.decision]?.[1]}>{DECISIONS[e.decision]?.[0] ?? e.decision}</Tag>;
const sender = (e: Entry) => e.sender || "<> (уведомление о недоставке)";
/** Карточка письма: все события журнала с этим Message-ID. */
function MessageCard({ id }: { id: string }) {
  const { page, loading, pager } = useCursorPages<Schemas["DeliveryResults"]>(URL, { message_id: id });
  const events = page?.events ?? [];
  return (
    <>
      <Descriptions column={1} size="small">
        <Descriptions.Item label="Message-ID">{id}</Descriptions.Item>
        <Descriptions.Item label="Отправитель">{[...new Set(events.map(sender))].join(", ")}</Descriptions.Item>
        <Descriptions.Item label="Получатели">{[...new Set(events.flatMap((e) => e.recipients))].join(", ")}</Descriptions.Item>
      </Descriptions>
      <Typography.Title level={5}>События</Typography.Title>
      <Timeline
        pending={loading && "Загрузка"}
        items={events.map((e) => ({
          color: DECISIONS[e.decision]?.[1] === "default" ? "gray" : DECISIONS[e.decision]?.[1],
          children: (
            <>
              <div>
                {moment(e.at)} — {kind(e)} {decision(e)}
              </div>
              {e.reason && <div>Причина: {e.reason}</div>}
              <div>Получатели: {e.recipients.join(", ")}</div>
              <Typography.Text type="secondary">
                Узел {e.node}
                {e.queue_id && `, запись очереди ${e.queue_id}`}
              </Typography.Text>
            </>
          ),
        }))}
      />
      {pager}
    </>
  );
}

export function DeliveryPage() {
  const [filters, setFilters] = useState<Filters>({});
  const [card, setCard] = useState<string | null>(null);
  const { page, loading, pager } = useCursorPages<Schemas["DeliveryResults"]>(URL, {
    sender: filters.sender?.trim(),
    recipient: filters.recipient?.trim(),
    message_id: filters.message_id?.trim().replace(/^<|>$/g, ""),
    ...interval(filters.range),
  });
  return (
    <List title="Журнал доставки" breadcrumb={false} headerButtons={() => null}>
      <Form<Filters> layout="inline" onFinish={setFilters} style={{ marginBottom: 16, rowGap: 8 }}>
        <Form.Item name="sender" label="Отправитель">
          <Input allowClear placeholder="точный адрес" />
        </Form.Item>
        <Form.Item name="recipient" label="Получатель">
          <Input allowClear placeholder="точный адрес" />
        </Form.Item>
        <Form.Item name="message_id" label="Message-ID">
          <Input allowClear />
        </Form.Item>
        <Form.Item name="range" label="Интервал">
          <DatePicker.RangePicker showTime allowEmpty={[true, true]} />
        </Form.Item>
        <Button type="primary" htmlType="submit">
          Найти
        </Button>
      </Form>
      <Table<Entry> dataSource={page?.events} loading={loading} rowKey={(e) => `${e.at} ${e.node} ${e.kind} ${e.queue_id} ${e.recipients}`} pagination={false}>
        <Table.Column<Entry> title="Время" render={(_, e) => moment(e.at)} />
        <Table.Column<Entry> title="Событие" render={(_, e) => kind(e)} />
        <Table.Column<Entry> title="Решение" render={(_, e) => decision(e)} />
        <Table.Column<Entry> title="Отправитель" render={(_, e) => sender(e)} />
        <Table.Column<Entry> title="Получатели" render={(_, e) => e.recipients.join(", ")} />
        <Table.Column<Entry>
          title="Message-ID"
          render={(_, e) =>
            e.message_id ? (
              <Button type="link" size="small" style={{ padding: 0 }} onClick={() => setCard(e.message_id ?? null)}>
                {e.message_id}
              </Button>
            ) : (
              "—"
            )
          }
        />
        <Table.Column<Entry> title="Причина" render={(_, e) => e.reason ?? ""} />
        <Table.Column dataIndex="node" title="Узел" />
      </Table>
      {pager}
      <Drawer open={card !== null} title="Письмо" width={640} onClose={() => setCard(null)} destroyOnHidden>
        {card !== null && <MessageCard id={card} />}
      </Drawer>
    </List>
  );
}
