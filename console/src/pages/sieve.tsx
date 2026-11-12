// Правила Sieve (SRS 4.3): скрипты и автоответ учётной записи (за пользователя), скрипт организации.
// Текст проверяет узел: при отказе 422 ошибки с позициями показываются под редактором.
import { List, Show } from "@refinedev/antd";
import { type BaseRecord, type HttpError, useCustom, useCustomMutation, useOne } from "@refinedev/core";
import { Alert, Button, DatePicker, Form, Input, Popconfirm, Space, Switch, Table, Tabs, Tag, Typography } from "antd";
import type { TextAreaRef } from "antd/es/input/TextArea";
import dayjs, { type Dayjs } from "dayjs";
import { useRef, useState } from "react";
import { useParams } from "react-router";

import type { Schemas } from "../api/client";
import { bytes } from "../components/format";
import { useReadOnly } from "../providers/access";

type Script = Schemas["Script"];
type ScriptError = Schemas["ScriptError"];
type Save = (name: string, text: string, done: () => void) => void;

const enc = encodeURIComponent;
const errorsOf = (e: HttpError): ScriptError[] => (e.problem as Partial<Schemas["ScriptProblem"]> | undefined)?.errors ?? [];

/** Редактор текста скрипта; ошибки проверки — список с позициями, нажатие выделяет строку с ошибкой в тексте. */
function ScriptEditor(p: { name?: string; text: string; errors: ScriptError[]; saving: boolean; readOnly: boolean; onSave: Save }) {
  const [name, setName] = useState(p.name ?? "");
  const [text, setText] = useState(p.text);
  const area = useRef<TextAreaRef>(null);
  const select = (line: number) => {
    const lines = text.split("\n");
    const start = lines.slice(0, line - 1).reduce((n, l) => n + l.length + 1, 0);
    const el = area.current?.resizableTextArea?.textArea;
    el?.focus();
    el?.setSelectionRange(start, start + (lines[line - 1]?.length ?? 0));
  };
  return (
    <Space direction="vertical" style={{ width: "100%" }}>
      {p.name === undefined && (
        <Input aria-label="Имя скрипта" addonBefore="Имя скрипта" value={name} onChange={(e) => setName(e.target.value)} />
      )}
      <Input.TextArea
        ref={area}
        aria-label="Текст скрипта"
        rows={16}
        spellCheck={false}
        readOnly={p.readOnly}
        status={p.errors.length ? "error" : undefined}
        style={{ fontFamily: "monospace" }}
        value={text}
        onChange={(e) => setText(e.target.value)}
      />
      {p.errors.length > 0 && (
        <Alert
          type="error"
          message="Скрипт не сохранён: ошибки в тексте"
          description={p.errors.map((e, i) => (
            <div key={i}>
              <Button type="link" size="small" style={{ padding: 0 }} onClick={() => select(e.line)}>
                Строка {e.line}, столбец {e.column}
              </Button>
              : {e.message}
              <pre style={{ margin: 0 }}>{text.split("\n")[e.line - 1]}</pre>
            </div>
          ))}
        />
      )}
      {!p.readOnly && (
        <Button type="primary" loading={p.saving} disabled={!name.trim() && p.name === undefined} onClick={() => p.onSave(p.name ?? name.trim(), text, () => undefined)}>
          Сохранить скрипт
        </Button>
      )}
    </Space>
  );
}

/** Скрипты учётной записи: список, редактор, активация (активный — не больше одного). */
function AccountScripts({ base, readOnly }: { base: string; readOnly: boolean }) {
  // `null` — редактор закрыт; `{}` — новый скрипт; `{ name }` — существующий.
  const [open, setOpen] = useState<{ name?: string } | null>(null);
  const [errors, setErrors] = useState<ScriptError[]>([]);
  const list = useCustom<Script[]>({ url: `${base}/sieve/scripts`, method: "get" });
  const body = useCustom<Schemas["ScriptBody"]>({
    url: `${base}/sieve/scripts/${enc(open?.name ?? "")}`,
    method: "get",
    queryOptions: { enabled: Boolean(open?.name), gcTime: 0 },
  });
  const { mutate, mutation } = useCustomMutation<BaseRecord, HttpError, unknown>();
  const run = (method: "post" | "put" | "delete", url: string, message: string, values?: unknown, done?: () => void) =>
    mutate(
      { url, method, values, successNotification: { type: "success", message } },
      {
        onSuccess: () => {
          setErrors([]);
          done?.();
          void list.query.refetch();
        },
        onError: (e) => setErrors(errorsOf(e)),
      },
    );
  const show = (o: { name?: string } | null) => {
    setErrors([]);
    setOpen(o);
  };
  const script = (name: string) => `${base}/sieve/scripts/${enc(name)}`;
  // До загрузки Refine отдаёт пустой объект, а не `undefined`.
  const scripts = list.query.isSuccess ? list.result.data : undefined;
  const active = scripts?.some((s) => s.active);
  const loaded = body.query.isSuccess && body.result.data.name === open?.name ? body.result.data : undefined;
  return (
    <>
      {!readOnly && (
        <Space style={{ marginBottom: 16 }}>
          <Button type="primary" onClick={() => show({})}>
            Новый скрипт
          </Button>
          {active && (
            <Popconfirm title="Отключить правила? Активного скрипта не останется." okText="Отключить" onConfirm={() => run("post", `${base}/sieve/deactivate`, "Правила отключены")}>
              <Button>Отключить правила</Button>
            </Popconfirm>
          )}
        </Space>
      )}
      <Table<Script> dataSource={scripts} loading={list.query.isLoading} rowKey="name" pagination={false}>
        <Table.Column dataIndex="name" title="Имя" />
        <Table.Column<Script> title="Размер" render={(_, s) => bytes(s.size)} />
        <Table.Column<Script> title="Состояние" render={(_, s) => (s.active ? <Tag color="green">активен</Tag> : "не выполняется")} />
        <Table.Column<Script>
          title="Действия"
          width={340}
          render={(_, s) => (
            <Space>
              <Button size="small" onClick={() => show({ name: s.name })}>
                Открыть
              </Button>
              {!readOnly && !s.active && (
                <>
                  <Button size="small" onClick={() => run("post", `${script(s.name)}/activate`, `Скрипт ${s.name} активирован`)}>
                    Активировать
                  </Button>
                  <Popconfirm
                    title={`Удалить скрипт ${s.name}?`}
                    okText="Удалить"
                    okButtonProps={{ danger: true }}
                    onConfirm={() => run("delete", script(s.name), `Скрипт ${s.name} удалён`, undefined, () => open?.name === s.name && show(null))}
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
      {open && (open.name === undefined || loaded) && (
        <div style={{ marginTop: 24 }}>
          <Typography.Title level={5}>{open.name === undefined ? "Новый скрипт" : `Скрипт ${open.name}`}</Typography.Title>
          <ScriptEditor
            key={open.name ?? ""}
            name={open.name}
            text={loaded?.text ?? ""}
            errors={errors}
            saving={mutation.isPending}
            readOnly={readOnly}
            onSave={(name, text) => run("put", script(name), `Скрипт ${name} сохранён`, { text }, () => show({ name }))}
          />
        </div>
      )}
    </>
  );
}

type VacationForm = { enabled: boolean; period?: [Dayjs | null, Dayjs | null] | null; subject?: string | null; text_body?: string | null; html_body?: string | null };

/** Автоответ за пользователя (модель JMAP VacationResponse); выполняется до активного скрипта. */
function Vacation({ base, readOnly }: { base: string; readOnly: boolean }) {
  const url = `${base}/vacation`;
  const { result, query } = useCustom<Schemas["VacationBody"]>({ url, method: "get" });
  const { mutate, mutation } = useCustomMutation<Schemas["VacationBody"], HttpError, Schemas["VacationBody"]>();
  if (!query.isSuccess) return null;
  const v = result.data;
  return (
    <Form<VacationForm>
      layout="vertical"
      disabled={readOnly}
      initialValues={{ ...v, enabled: Boolean(v.enabled), period: [v.from ? dayjs(v.from) : null, v.to ? dayjs(v.to) : null] }}
      onFinish={(f) =>
        mutate(
          {
            url,
            method: "put",
            values: {
              enabled: f.enabled,
              from: f.period?.[0]?.toISOString() ?? null,
              to: f.period?.[1]?.toISOString() ?? null,
              subject: f.subject?.trim() || null,
              text_body: f.text_body || null,
              html_body: f.html_body || null,
            },
            successNotification: { type: "success", message: "Автоответ сохранён" },
          },
          { onSuccess: () => void query.refetch() },
        )
      }
    >
      <Form.Item name="enabled" label="Автоответ включён" valuePropName="checked">
        <Switch />
      </Form.Item>
      <Form.Item name="period" label="Период действия" extra="Пустое начало — сразу; пустой конец — бессрочно.">
        <DatePicker.RangePicker showTime allowEmpty={[true, true]} />
      </Form.Item>
      <Form.Item name="subject" label="Тема" extra="Пусто — «Автоответ: <тема письма>».">
        <Input />
      </Form.Item>
      <Form.Item name="text_body" label="Текст ответа">
        <Input.TextArea rows={6} />
      </Form.Item>
      <Form.Item name="html_body" label="Текст ответа HTML">
        <Input.TextArea rows={6} style={{ fontFamily: "monospace" }} />
      </Form.Item>
      {!readOnly && (
        <Button type="primary" htmlType="submit" loading={mutation.isPending}>
          Сохранить автоответ
        </Button>
      )}
    </Form>
  );
}

/** Правила и автоответ учётной записи. */
export function AccountSieve() {
  const { id = "" } = useParams();
  const base = `/api/v1/orgs/{org}/accounts/${enc(id)}`;
  const readOnly = useReadOnly("accounts");
  const { result: account } = useOne<Schemas["AccountInfo"]>({ resource: "accounts", id });
  return (
    <Show title={`Правила и автоответ: ${account?.login ?? ""}`} breadcrumb={false} headerButtons={() => null}>
      <Tabs
        destroyOnHidden
        items={[
          { key: "scripts", label: "Правила Sieve", children: <AccountScripts base={base} readOnly={readOnly} /> },
          { key: "vacation", label: "Автоответ", children: <Vacation base={base} readOnly={readOnly} /> },
        ]}
      />
    </Show>
  );
}

const ORG_SCRIPT = "/api/v1/orgs/{org}/sieve";

/** Скрипт организации: административные правила для входящей почты всех ящиков организации. */
export function OrgSieve() {
  const readOnly = useReadOnly("sieve");
  const [errors, setErrors] = useState<ScriptError[]>([]);
  // Скрипта нет — узел отвечает 404: это пустой редактор, а не ошибка.
  const { result, query } = useCustom<Schemas["ScriptText"]>({
    url: ORG_SCRIPT,
    method: "get",
    errorNotification: false,
    queryOptions: { retry: false },
  });
  const { mutate, mutation } = useCustomMutation<BaseRecord, HttpError, unknown>();
  const run = (method: "put" | "delete", message: string, values?: unknown) =>
    mutate(
      { url: ORG_SCRIPT, method, values, successNotification: { type: "success", message } },
      {
        onSuccess: () => {
          setErrors([]);
          void query.refetch();
        },
        onError: (e) => setErrors(errorsOf(e)),
      },
    );
  const exists = query.isSuccess;
  return (
    <List title="Правила организации" breadcrumb={false} headerButtons={() => null}>
      <Typography.Paragraph type="secondary">
        Скрипт Sieve организации выполняется при доставке для всех ящиков организации. {query.isError && "Сейчас скрипта нет."}
      </Typography.Paragraph>
      {!query.isLoading && (
        <ScriptEditor
          key={`${exists} ${query.dataUpdatedAt}`}
          name="организации"
          text={exists ? (result.data?.text ?? "") : ""}
          errors={errors}
          saving={mutation.isPending}
          readOnly={readOnly}
          onSave={(_, text) => run("put", "Скрипт организации сохранён", { text })}
        />
      )}
      {exists && !readOnly && (
        <Popconfirm title="Удалить скрипт организации?" okText="Удалить" okButtonProps={{ danger: true }} onConfirm={() => run("delete", "Скрипт организации удалён")}>
          <Button danger style={{ marginTop: 16 }}>
            Удалить скрипт
          </Button>
        </Popconfirm>
      )}
    </List>
  );
}
