// Очередь модерации организации или одной группы (SRS 4.5): одобрить — доставка, отклонить — уведомление отправителю.
import { useCustomMutation, useList } from "@refinedev/core";
import { Button, Input, Modal, Space, Table } from "antd";
import { useState } from "react";

import type { Schemas } from "../api/client";

import { bytes, when } from "./format";
import { useSubjects } from "./subjects";

type Held = Schemas["HeldInfo"];

export function ModerationQueue({ group }: { group?: string }) {
  const { include, shown, toggle } = useSubjects("moderation");
  const { result, query } = useList<Held>({
    resource: "moderation",
    pagination: { mode: "off" },
    filters: [
      ...(group ? [{ field: "group", operator: "eq" as const, value: group }] : []),
      ...(include ? [{ field: "include", operator: "eq" as const, value: include }] : []),
    ],
  });
  const { result: groups } = useList<Schemas["GroupInfo"]>({ resource: "groups", pagination: { mode: "off" } });
  const { mutate, mutation } = useCustomMutation<never, never, Schemas["Rejection"] | undefined>();
  const [rejecting, setRejecting] = useState<Held>();
  const [reason, setReason] = useState("");
  const decide = (h: Held, action: "approve" | "reject", values?: Schemas["Rejection"]) =>
    mutate(
      {
        url: `/api/v1/orgs/{org}/moderation/${encodeURIComponent(h.id)}/${action}`,
        method: "post",
        values,
        successNotification: { type: "success", message: action === "approve" ? "Письмо одобрено" : "Письмо отклонено" },
      },
      {
        onSuccess: () => {
          setRejecting(undefined);
          setReason("");
          void query.refetch();
        },
      },
    );
  const cause = (h: Held) =>
    h.source === "flow_rule"
      ? `правило потока «${h.rule ?? ""}»`
      : `группа «${groups.data.find((g) => g.id === h.group)?.name ?? h.group ?? ""}»`;
  return (
    <>
      {toggle}
      <Table<Held> dataSource={result.data} loading={query.isLoading} rowKey="id" pagination={false}>
        <Table.Column dataIndex="sender" title="Отправитель" />
        {shown && <Table.Column<Held> title="Тема" render={(_, h) => h.subject ?? "—"} />}
        <Table.Column<Held> title="Получатели" render={(_, h) => h.recipients.join(", ")} />
        <Table.Column<Held> title="Причина задержки" render={(_, h) => cause(h)} />
        <Table.Column<Held> title="Размер" render={(_, h) => bytes(h.size)} />
        <Table.Column<Held> title="В очереди с" render={(_, h) => when(h.held_at)} />
        <Table.Column<Held>
          title="Действия"
          width={240}
          render={(_, h) => (
            <Space>
              <Button size="small" type="primary" loading={mutation.isPending} onClick={() => decide(h, "approve")}>
                Одобрить
              </Button>
              <Button size="small" danger onClick={() => setRejecting(h)}>
                Отклонить
              </Button>
            </Space>
          )}
        />
      </Table>
      <Modal
        open={!!rejecting}
        title={`Отклонить письмо от ${rejecting?.sender ?? ""}?`}
        okText="Отклонить письмо"
        okButtonProps={{ danger: true }}
        confirmLoading={mutation.isPending}
        onOk={() => rejecting && decide(rejecting, "reject", { reason: reason.trim() || null })}
        onCancel={() => setRejecting(undefined)}
      >
        <Input
          aria-label="Причина отклонения"
          placeholder="Причина для уведомления отправителя (необязательно)"
          maxLength={500}
          value={reason}
          onChange={(e) => setReason(e.target.value)}
        />
      </Modal>
    </>
  );
}
