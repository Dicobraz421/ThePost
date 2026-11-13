// Оргструктура: дерево подразделений организации (по `parent`) и сотрудники подразделения (ADM-10).
import { List } from "@refinedev/antd";
import { type HttpError, useCreate, useCustom, useDelete, useList, useUpdate } from "@refinedev/core";
import { Button, Drawer, Form, Input, Modal, Popconfirm, Space, Switch, Table, Tag, TreeSelect } from "antd";
import { useState } from "react";
import { Link } from "react-router";

import type { Schemas } from "../api/client";
import { useReadOnly } from "../providers/access";

type Unit = Schemas["UnitInfo"];
type Node = Unit & { children?: Node[] };
type Member = Schemas["UnitMember"];

/** Дерево по `parent`; подразделение с вышестоящим, которого нет в списке, — корневое. */
export function tree(units: Unit[]): Node[] {
  const ids = new Set(units.map((u) => u.id));
  const build = (parent: string | null): Node[] =>
    units
      .filter((u) => (u.parent && ids.has(u.parent) ? u.parent : null) === parent)
      .sort((a, b) => a.name.localeCompare(b.name, "ru"))
      .map((u) => {
        const children = build(u.id);
        return children.length ? { ...u, children } : { ...u };
      });
  return build(null);
}

/** Варианты вышестоящего: всё дерево, кроме самого подразделения и его вложенных. */
type Choice = { value: string; title: string; children?: Choice[] };
const parents = (nodes: Node[], self?: string): Choice[] =>
  nodes.filter((n) => n.id !== self).map((n) => ({ value: n.id, title: n.name, children: n.children && parents(n.children, self) }));

function Members({ unit }: { unit: Unit }) {
  const [subtree, setSubtree] = useState(false);
  // До ответа `result.data` — пустой объект: список — из ответа запроса.
  const { query } = useCustom<Member[]>({
    url: `/api/v1/orgs/{org}/units/${encodeURIComponent(unit.id)}/members`,
    method: "get",
    config: { query: { subtree: subtree || undefined } },
  });
  return (
    <>
      <Space style={{ marginBottom: 16 }}>
        <Switch aria-label="С вложенными подразделениями" checked={subtree} onChange={setSubtree} />
        <span>С вложенными подразделениями</span>
      </Space>
      <Table<Member> dataSource={query.data?.data} loading={query.isLoading} rowKey="account" pagination={false} size="small">
        <Table.Column<Member>
          title="Учётная запись"
          render={(_, m) => <Link to={`/accounts/${encodeURIComponent(m.account)}`}>{m.account}</Link>}
        />
        <Table.Column<Member> title="Должность" render={(_, m) => m.title ?? "—"} />
        <Table.Column<Member> title="Отдел" render={(_, m) => m.department ?? "—"} />
        <Table.Column<Member> title="Руководитель" render={(_, m) => m.manager ?? "—"} />
        <Table.Column<Member> title="Старшинство" render={(_, m) => m.seniority ?? "—"} />
      </Table>
    </>
  );
}

/** Окно подразделения (`unit` — `null` для нового): форма создаётся заново при каждом открытии. */
function UnitModal(p: { unit: Unit | null; nodes: Node[]; saving: boolean; onSave: (v: Schemas["UnitBody"]) => void; onClose: () => void }) {
  const [form] = Form.useForm<Schemas["UnitBody"]>();
  return (
    <Modal
      open
      title={p.unit ? `Подразделение «${p.unit.name}»` : "Новое подразделение"}
      okText="Сохранить"
      confirmLoading={p.saving}
      onOk={() => form.submit()}
      onCancel={p.onClose}
    >
      <Form<Schemas["UnitBody"]>
        form={form}
        layout="vertical"
        initialValues={{ name: p.unit?.name ?? "", parent: p.unit?.parent ?? null }}
        onFinish={p.onSave}
      >
        <Form.Item name="name" label="Название" rules={[{ required: true, whitespace: true, message: "Укажите название" }]}>
          <Input maxLength={255} />
        </Form.Item>
        <Form.Item name="parent" label="Вышестоящее подразделение" extra="Не задано — корневое.">
          <TreeSelect allowClear treeDefaultExpandAll treeData={parents(p.nodes, p.unit?.id)} />
        </Form.Item>
      </Form>
    </Modal>
  );
}

export function UnitsPage() {
  const readOnly = useReadOnly("units");
  const { result, query } = useList<Unit>({ resource: "units", pagination: { mode: "off" } });
  const nodes = tree(result.data);
  // Окно подразделения: `null` — новое, `undefined` — закрыто.
  const [editing, setEditing] = useState<Unit | null>();
  const [members, setMembers] = useState<Unit>();
  const { mutate: create, mutation: creating } = useCreate<Unit, HttpError, Schemas["UnitBody"]>();
  const { mutate: update, mutation: updating } = useUpdate<Unit, HttpError, Schemas["UnitBody"]>();
  const { mutate: remove } = useDelete();
  const save = (v: Schemas["UnitBody"]) => {
    const values = { name: v.name.trim(), parent: v.parent ?? null };
    const done = { onSuccess: () => setEditing(undefined) };
    if (editing) update({ resource: "units", id: editing.id, values, successNotification: { type: "success", message: "Подразделение сохранено" } }, done);
    else create({ resource: "units", values, successNotification: { type: "success", message: "Подразделение создано" } }, done);
  };
  return (
    <List
      title="Оргструктура"
      headerButtons={() =>
        !readOnly && (
          <Button type="primary" onClick={() => setEditing(null)}>
            Новое подразделение
          </Button>
        )
      }
    >
      {/* Дерево раскрыто целиком: таблица создаётся заново, когда меняется набор подразделений. */}
      <Table<Node>
        key={result.data.map((u) => u.id).join()}
        dataSource={nodes}
        loading={query.isLoading}
        rowKey="id"
        pagination={false}
        expandable={{ defaultExpandAllRows: true }}
      >
        <Table.Column dataIndex="name" title="Подразделение" />
        <Table.Column<Node> title="Источник" render={(_, u) => (u.external_id ? <Tag>каталог</Tag> : "локальное")} />
        <Table.Column<Node>
          title="Действия"
          width={360}
          render={(_, u) => (
            <Space>
              <Button size="small" onClick={() => setMembers(u)}>
                Сотрудники
              </Button>
              {!readOnly && !u.external_id && (
                <>
                  <Button size="small" onClick={() => setEditing(u)}>
                    Изменить
                  </Button>
                  <Popconfirm
                    title={`Удалить подразделение «${u.name}»?`}
                    description="Подразделение с вложенными удалить нельзя."
                    okText="Удалить"
                    onConfirm={() => remove({ resource: "units", id: u.id, successNotification: { type: "success", message: "Подразделение удалено" } })}
                  >
                    <Button size="small" danger>
                      Удалить
                    </Button>
                  </Popconfirm>
                </>
              )}
            </Space>
          )}
        />
      </Table>
      {editing !== undefined && (
        <UnitModal unit={editing} nodes={nodes} saving={creating.isPending || updating.isPending} onSave={save} onClose={() => setEditing(undefined)} />
      )}
      <Drawer open={!!members} title={`Сотрудники: ${members?.name ?? ""}`} width={720} onClose={() => setMembers(undefined)} destroyOnHidden>
        {members && <Members unit={members} />}
      </Drawer>
    </List>
  );
}
