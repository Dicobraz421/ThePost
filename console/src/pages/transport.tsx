// Транспорт (SRS 4.1): принятые домены — единственный ресурс доменов организации (решение 94) — и их политика,
// доверенные источники, политики доменов назначения (маршруты).
// Каждая запись несёт версию снимка из последнего чтения; расхождение — ответ 409, списки перечитываются.
import { List } from "@refinedev/antd";
import { type BaseRecord, type HttpError, useCustom, useCustomMutation, usePermissions } from "@refinedev/core";
import { Button, Form, Input, Modal, Popconfirm, Select, Space, Spin, Switch, Table, Tabs, Typography } from "antd";
import { type ReactNode, useState } from "react";

import type { Schemas } from "../api/client";
import { useOrgs } from "../components/orgs";
import { PolicyForm } from "../components/policy-form";
import { useReadOnly } from "../providers/access";

// Схема OpenAPI описывает эти значения как произвольные объекты (`DomainList`, `SourceList`, `RouteList`,
// `ClusterView`); типы — по контракту `tp-contracts::transport`.
type DomainKind = "authoritative" | { internal_relay: { next_hop: string } };
type Domain = { id: string; tenant: string; name: string; kind: DomainKind };
type Matcher = { networks: string[] } | { client_cert: { subject: string } };
type SourceKind =
  | { gateway: { xclient: boolean; authserv_id: string | null } }
  | { application: { senders: string[]; external: boolean } };
type Source = { name: string; tenant: string | null; matcher: Matcher; kind: SourceKind };
type Tls = "opportunistic" | "required" | { pinned: { spki_sha256: string[] } };
type Route = { pattern: string; tls: Tls; smart_host: string | null; egress_ip: string | null };

type Write = { method: "post" | "put" | "delete"; url: string; values?: unknown };
type Run = (w: Write, message: string, done?: () => void) => void;

const ORG = "/api/v1/orgs/{org}/transport";
const CLUSTER = "/api/v1/transport";
const enc = encodeURIComponent;
const blank = (v?: string | null) => v?.trim() || null;
const required = (message: string) => [{ required: true, message }];

type SectionProps<T, F> = {
  /** Что добавляется: «принятый домен». */
  what: string;
  rows?: T[];
  loading: boolean;
  rowKey: (r: T) => string;
  columns: ReactNode;
  fields: (editing: boolean) => ReactNode;
  initial: Partial<F>;
  toForm: (r: T) => F;
  save: (v: F, row?: T) => Write;
  remove: (r: T) => Write;
  /** Нельзя изменить из этого раздела (чужая запись, только чтение). */
  locked?: (r: T) => boolean;
  /** Дополнительное действие строки (доступно и только для чтения). */
  extra?: (r: T) => ReactNode;
  readOnly: boolean;
  saving: boolean;
  run: Run;
};

/** Таблица записей с добавлением и изменением в окне и удалением с подтверждением. */
function Section<T extends object, F extends object>(p: SectionProps<T, F>) {
  const [open, setOpen] = useState<{ row?: T } | null>(null);
  const [form] = Form.useForm<F>();
  return (
    <>
      {!p.readOnly && (
        <Button type="primary" style={{ marginBottom: 16 }} onClick={() => setOpen({})}>
          Добавить {p.what}
        </Button>
      )}
      <Table<T> dataSource={p.rows} loading={p.loading} rowKey={p.rowKey} pagination={false}>
        {p.columns}
        {(!p.readOnly || p.extra) && (
          <Table.Column<T>
            title="Действия"
            width={p.extra ? 320 : 220}
            render={(_, r) => (
              <Space>
                {p.extra?.(r)}
                {!p.readOnly && !p.locked?.(r) && (
                  <>
                    <Button size="small" onClick={() => setOpen({ row: r })}>
                      Изменить
                    </Button>
                    <Popconfirm
                      title={`Удалить ${p.what} ${p.rowKey(r)}?`}
                      okText="Удалить"
                      okButtonProps={{ danger: true }}
                      onConfirm={() => p.run(p.remove(r), `Удалено: ${p.rowKey(r)}`)}
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
        )}
      </Table>
      <Modal
        open={open !== null}
        title={open?.row ? `Изменить: ${p.rowKey(open.row)}` : `Добавить ${p.what}`}
        okText="Сохранить"
        confirmLoading={p.saving}
        onOk={() => form.submit()}
        onCancel={() => setOpen(null)}
        destroyOnHidden
      >
        <Form<F>
          form={form}
          layout="vertical"
          preserve={false}
          initialValues={open?.row ? p.toForm(open.row) : p.initial}
          onFinish={(v) => p.run(p.save(v, open?.row), "Сохранено", () => setOpen(null))}
        >
          {p.fields(Boolean(open?.row))}
        </Form>
      </Modal>
    </>
  );
}

// --- Принятые домены -------------------------------------------------------------------------------------------

type DomainForm = { name: string; kind: "authoritative" | "internal_relay"; next_hop?: string };

const domainKind = (k: DomainKind) =>
  k === "authoritative" ? "полномочный" : `внутренняя ретрансляция → ${k.internal_relay.next_hop}`;

function DomainFields() {
  const kind = Form.useWatch("kind", Form.useFormInstance<DomainForm>());
  return (
    <>
      <Form.Item name="name" label="Имя домена" rules={required("Укажите имя домена")}>
        <Input placeholder="example.org" />
      </Form.Item>
      <Form.Item
        name="kind"
        label="Вид"
        extra="Полномочный: неизвестный адрес отклоняется. Внутренняя ретрансляция: неизвестные адреса передаются на другой узел (совместная работа со старым сервером)."
      >
        <Select
          options={[
            { value: "authoritative", label: "полномочный" },
            { value: "internal_relay", label: "внутренняя ретрансляция" },
          ]}
        />
      </Form.Item>
      {kind === "internal_relay" && (
        <Form.Item name="next_hop" label="Узел назначения" rules={required("Укажите узел")}>
          <Input placeholder="old-mail.example.org:25" />
        </Form.Item>
      )}
    </>
  );
}

/** Политика принятого домена — уровень «домен» цепочки организация → домен → ящик. */
function DomainPolicy({ domain, onClose }: { domain: Domain; onClose: () => void }) {
  const url = `${ORG}/domains/${enc(domain.id)}/policy`;
  const { result, query } = useCustom<Schemas["Policies"]>({ url, method: "get" });
  const { mutate, mutation } = useCustomMutation<Schemas["PolicyBody"]>();
  return (
    <Modal open title={`Политика домена ${domain.name}`} footer={null} width={720} onCancel={onClose} destroyOnHidden>
      {query.isLoading ? (
        <Spin />
      ) : (
        <PolicyForm
          policies={result.data}
          saving={mutation.isPending}
          onSave={(values) =>
            mutate(
              { url, method: "put", values, successNotification: { type: "success", message: "Политика домена сохранена" } },
              { onSuccess: () => void query.refetch() },
            )
          }
        />
      )}
    </Modal>
  );
}

const domainColumns = (
  <>
    <Table.Column dataIndex="name" title="Имя" />
    <Table.Column<Domain> title="Вид" render={(_, d) => domainKind(d.kind)} />
  </>
);

// --- Доверенные источники ----------------------------------------------------------------------------------------

type SourceForm = {
  name: string;
  match: "networks" | "client_cert";
  networks?: string[];
  subject?: string;
  kind: "gateway" | "application";
  xclient?: boolean;
  authserv_id?: string | null;
  senders?: string[];
  external?: boolean;
};

const matcher = (m: Matcher) => ("networks" in m ? m.networks.join(", ") : `сертификат: ${m.client_cert.subject}`);
const sourceKind = (k: SourceKind) =>
  "gateway" in k
    ? `шлюз${k.gateway.xclient ? ", XCLIENT" : ""}${k.gateway.authserv_id ? `, authserv-id ${k.gateway.authserv_id}` : ""}`
    : `приложение: ${k.application.senders.join(", ")}${k.application.external ? ", внешние получатели" : ""}`;

const sourceForm = (s: Source): SourceForm => ({
  name: s.name,
  ...("networks" in s.matcher
    ? { match: "networks", networks: s.matcher.networks }
    : { match: "client_cert", subject: s.matcher.client_cert.subject }),
  ...("gateway" in s.kind ? { kind: "gateway", ...s.kind.gateway } : { kind: "application", ...s.kind.application }),
});

const sourceBody = (v: SourceForm, version: number) => ({
  version,
  matcher: v.match === "networks" ? { networks: v.networks ?? [] } : { client_cert: { subject: v.subject?.trim() } },
  kind:
    v.kind === "gateway"
      ? { gateway: { xclient: Boolean(v.xclient), authserv_id: blank(v.authserv_id) } }
      : { application: { senders: v.senders ?? [], external: Boolean(v.external) } },
});

/** `cluster` — источник кластера: только общий шлюз. */
function SourceFields({ editing, cluster }: { editing: boolean; cluster?: boolean }) {
  const form = Form.useFormInstance<SourceForm>();
  const match = Form.useWatch("match", form);
  const kind = Form.useWatch("kind", form);
  return (
    <>
      <Form.Item name="name" label="Имя источника" extra="Строчные латинские буквы, цифры, «.», «_», «-»; до 64 символов." rules={required("Укажите имя")}>
        <Input disabled={editing} placeholder="mx-gateway" />
      </Form.Item>
      <Form.Item name="match" label="Признак соединения">
        <Select
          options={[
            { value: "networks", label: "адрес клиента в подсетях" },
            { value: "client_cert", label: "сертификат клиента (mTLS)" },
          ]}
        />
      </Form.Item>
      {match === "networks" ? (
        <Form.Item name="networks" label="Подсети" rules={required("Укажите хотя бы одну подсеть")}>
          <Select mode="tags" open={false} suffixIcon={null} placeholder="192.0.2.0/24 — Enter" />
        </Form.Item>
      ) : (
        <Form.Item name="subject" label="Имя в сертификате (dNSName)" rules={required("Укажите имя")}>
          <Input placeholder="mx.example.org" />
        </Form.Item>
      )}
      <Form.Item name="kind" label="Вид" hidden={cluster}>
        <Select
          options={[
            { value: "gateway", label: "шлюз (внешний MTA перед сервером)" },
            { value: "application", label: "приложение или устройство без входа (МФУ, учётная система)" },
          ]}
        />
      </Form.Item>
      {kind === "gateway" ? (
        <>
          <Form.Item name="xclient" label="Принимать исходный адрес клиента (XCLIENT/XFORWARD)" valuePropName="checked">
            <Switch />
          </Form.Item>
          <Form.Item name="authserv_id" label="Доверять проверкам шлюза (authserv-id)" extra="Пусто — результатам проверок шлюза не доверять.">
            <Input placeholder="mx.example.org" />
          </Form.Item>
        </>
      ) : (
        <>
          <Form.Item name="senders" label="Разрешённые адреса отправителя" rules={required("Укажите хотя бы один адрес или домен")}>
            <Select mode="tags" open={false} suffixIcon={null} placeholder="scanner@example.org или @example.org — Enter" />
          </Form.Item>
          <Form.Item name="external" label="Разрешены внешние получатели" valuePropName="checked">
            <Switch />
          </Form.Item>
        </>
      )}
    </>
  );
}

const sourceColumns = (
  <>
    <Table.Column dataIndex="name" title="Имя" />
    <Table.Column<Source> title="Признак соединения" render={(_, s) => matcher(s.matcher)} />
    <Table.Column<Source> title="Вид" render={(_, s) => sourceKind(s.kind)} />
  </>
);

// --- Маршруты ----------------------------------------------------------------------------------------------------

type RouteForm = { pattern: string; tls: "opportunistic" | "required" | "pinned"; spki?: string[]; smart_host?: string | null; egress_ip?: string | null };

const tls = (t: Tls) =>
  typeof t === "string" ? { opportunistic: "по возможности", required: "обязательный" }[t] : `закреплённый ключ (${t.pinned.spki_sha256.length})`;

function RouteFields({ editing }: { editing: boolean }) {
  const mode = Form.useWatch("tls", Form.useFormInstance<RouteForm>());
  return (
    <>
      <Form.Item
        name="pattern"
        label="Домен назначения"
        extra="example.org — домен, *.example.org — его поддомены, * — все внешние домены."
        rules={required("Укажите шаблон")}
      >
        <Input disabled={editing} placeholder="example.org" />
      </Form.Item>
      <Form.Item name="tls" label="TLS">
        <Select
          options={[
            { value: "opportunistic", label: "по возможности (MTA-STS и DANE — как обычно)" },
            { value: "required", label: "обязательный, с проверкой сертификата" },
            { value: "pinned", label: "обязательный, с закреплённым ключом" },
          ]}
        />
      </Form.Item>
      {mode === "pinned" && (
        <Form.Item name="spki" label="Отпечатки ключей (SHA-256 SPKI, hex)" rules={required("Укажите хотя бы один отпечаток")}>
          <Select mode="tags" open={false} suffixIcon={null} placeholder="64 символа — Enter" />
        </Form.Item>
      )}
      <Form.Item name="smart_host" label="Промежуточный узел" extra="Пусто — доставка по MX.">
        <Input placeholder="relay.example.net:587" />
      </Form.Item>
      <Form.Item name="egress_ip" label="Адрес исходящих соединений" extra="Пусто — адрес узла по умолчанию.">
        <Input placeholder="192.0.2.10" />
      </Form.Item>
    </>
  );
}

// --- Страница ----------------------------------------------------------------------------------------------------

export function TransportPage() {
  const { data: roles } = usePermissions<Schemas["AdminRole"][]>({});
  const installation = Boolean(roles?.includes("installation_admin"));
  const readOnly = useReadOnly("transport");
  const orgs = useOrgs();
  const domains = useCustom<{ version: number; domains: Domain[] }>({ url: `${ORG}/domains`, method: "get" });
  const sources = useCustom<{ version: number; sources: Source[] }>({ url: `${ORG}/sources`, method: "get" });
  const routes = useCustom<{ version: number; routes: Route[] }>({ url: `${ORG}/routes`, method: "get" });
  const cluster = useCustom<{ version: number; domains: Domain[]; sources: Source[] }>({ url: CLUSTER, method: "get", queryOptions: { enabled: installation } });
  const { mutate, mutation } = useCustomMutation<BaseRecord, HttpError, unknown>();
  const [policyOf, setPolicyOf] = useState<Domain | null>(null);
  const run: Run = (w, message, done) =>
    mutate(
      { ...w, values: w.values, successNotification: { type: "success", message } },
      {
        onSuccess: done,
        // Домены и источники делят версию снимка кластера: после записи (и после отказа 409) перечитывается всё.
        onSettled: () => {
          for (const q of [domains, sources, routes]) void q.query.refetch();
          if (installation) void cluster.query.refetch();
        },
      },
    );
  const common = { readOnly, saving: mutation.isPending, run };
  // Версия снимка кластера одна: берётся из самого свежего чтения.
  const clusterVersion = Math.max(domains.result.data?.version ?? 0, sources.result.data?.version ?? 0, cluster.result.data?.version ?? 0);
  const routesVersion = routes.result.data?.version ?? 0;
  const sourceSection = (base: string, rows: Source[] | undefined, loading: boolean, own: boolean) => (
    <Section<Source, SourceForm>
      {...common}
      what="доверенный источник"
      rows={rows}
      loading={loading}
      rowKey={(s) => s.name}
      columns={
        <>
          {sourceColumns}
          {!own && <Table.Column<Source> title="Владелец" render={(_, s) => (s.tenant ? orgs.name(s.tenant) : "кластер")} />}
        </>
      }
      fields={(editing) => <SourceFields editing={editing} cluster={!own} />}
      initial={{ match: "networks", kind: "gateway", xclient: false, external: false }}
      toForm={sourceForm}
      locked={(s) => !own && s.tenant !== null}
      save={(v, row) => ({ method: "put", url: `${base}/sources/${enc(row?.name ?? v.name.trim())}`, values: sourceBody(v, clusterVersion) })}
      remove={(s) => ({ method: "delete", url: `${base}/sources/${enc(s.name)}?version=${clusterVersion}` })}
    />
  );
  const items = [
    {
      key: "domains",
      label: "Принятые домены",
      children: (
        <Section<Domain, DomainForm>
          {...common}
          what="принятый домен"
          rows={domains.result.data?.domains}
          loading={domains.query.isLoading}
          rowKey={(d) => d.name}
          columns={domainColumns}
          fields={() => <DomainFields />}
          initial={{ kind: "authoritative" }}
          toForm={(d) => (d.kind === "authoritative" ? { name: d.name, kind: d.kind } : { name: d.name, kind: "internal_relay", ...d.kind.internal_relay })}
          save={(v, row) => ({
            method: row ? "put" : "post",
            url: row ? `${ORG}/domains/${enc(row.id)}` : `${ORG}/domains`,
            values: {
              version: clusterVersion,
              name: v.name.trim(),
              kind: v.kind === "internal_relay" ? { internal_relay: { next_hop: v.next_hop?.trim() } } : "authoritative",
            },
          })}
          remove={(d) => ({ method: "delete", url: `${ORG}/domains/${enc(d.id)}?version=${clusterVersion}` })}
          extra={(d) => (
            <Button size="small" onClick={() => setPolicyOf(d)}>
              Политика
            </Button>
          )}
        />
      ),
    },
    {
      key: "sources",
      label: "Доверенные источники",
      children: sourceSection(ORG, sources.result.data?.sources, sources.query.isLoading, true),
    },
    {
      key: "routes",
      label: "Маршруты",
      children: (
        <Section<Route, RouteForm>
          {...common}
          what="маршрут"
          rows={routes.result.data?.routes}
          loading={routes.query.isLoading}
          rowKey={(r) => r.pattern}
          columns={
            <>
              <Table.Column dataIndex="pattern" title="Домен назначения" />
              <Table.Column<Route> title="TLS" render={(_, r) => tls(r.tls)} />
              <Table.Column<Route> title="Промежуточный узел" render={(_, r) => r.smart_host ?? "по MX"} />
              <Table.Column<Route> title="Адрес исходящих соединений" render={(_, r) => r.egress_ip ?? "по умолчанию"} />
            </>
          }
          fields={(editing) => <RouteFields editing={editing} />}
          initial={{ tls: "opportunistic" }}
          toForm={(r) => ({
            pattern: r.pattern,
            smart_host: r.smart_host,
            egress_ip: r.egress_ip,
            ...(typeof r.tls === "string" ? { tls: r.tls } : { tls: "pinned", spki: r.tls.pinned.spki_sha256 }),
          })}
          save={(v, row) => ({
            method: "put",
            url: `${ORG}/routes/${enc(row?.pattern ?? v.pattern.trim())}`,
            values: {
              version: routesVersion,
              tls: v.tls === "pinned" ? { pinned: { spki_sha256: v.spki ?? [] } } : v.tls,
              smart_host: blank(v.smart_host),
              egress_ip: blank(v.egress_ip),
            },
          })}
          remove={(r) => ({ method: "delete", url: `${ORG}/routes/${enc(r.pattern)}?version=${routesVersion}` })}
        />
      ),
    },
    ...(installation
      ? [
          {
            key: "cluster",
            label: "Инсталляция",
            children: (
              <>
                <Typography.Title level={5}>Доверенные источники: общие шлюзы кластера и источники организаций</Typography.Title>
                {sourceSection(CLUSTER, cluster.result.data?.sources, cluster.query.isLoading, false)}
                <Typography.Title level={5} style={{ marginTop: 24 }}>
                  Принятые домены всех организаций
                </Typography.Title>
                <Table<Domain> dataSource={cluster.result.data?.domains} rowKey="id" pagination={false}>
                  {domainColumns}
                  <Table.Column<Domain> title="Организация" render={(_, d) => orgs.name(d.tenant)} />
                </Table>
              </>
            ),
          },
        ]
      : []),
  ];
  return (
    <List title="Транспорт" breadcrumb={false} headerButtons={() => null}>
      <Tabs destroyOnHidden items={items} />
      {policyOf && <DomainPolicy domain={policyOf} onClose={() => setPolicyOf(null)} />}
    </List>
  );
}
