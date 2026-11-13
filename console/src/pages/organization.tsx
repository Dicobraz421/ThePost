import { Show } from "@refinedev/antd";
import { type HttpError, useCustom, useCustomMutation } from "@refinedev/core";
import { Alert, Button, Descriptions, Divider, Popconfirm } from "antd";

import type { Schemas } from "../api/client";
import { when } from "../components/format";
import { PolicyForm } from "../components/policy-form";
import { useReadOnly } from "../providers/access";

const ORG = "/api/v1/orgs/{org}";

/**
 * Синхронизация каталога, остановленная массовым отключением учётных записей (ADM-10): подтверждение — следующая
 * синхронизация отключит их. Нет остановки — узел отвечает 404, предупреждения нет.
 */
function SyncHold() {
  const readOnly = useReadOnly("organization");
  const url = `${ORG}/sync-hold`;
  const { result, query } = useCustom<Schemas["SyncHold"]>({ url, method: "get", errorNotification: false, queryOptions: { retry: false } });
  const { mutate, mutation } = useCustomMutation<Schemas["SyncHold"], HttpError, undefined>();
  const hold = query.isError ? undefined : result.data;
  if (!hold?.detected_at) return null;
  const what = `к отключению ${hold.removed} из ${hold.total} учётных записей, обнаружено ${when(hold.detected_at)}`;
  return hold.approved ? (
    <Alert type="info" showIcon style={{ marginBottom: 16 }} message={`Отключение подтверждено: его выполнит следующая синхронизация каталога (${what}).`} />
  ) : (
    <Alert
      type="warning"
      showIcon
      style={{ marginBottom: 16 }}
      message={`Синхронизация каталога остановлена: ${what}.`}
      description="Каталог убрал больше учётных записей, чем допускает порог. Проверьте каталог; если удаление намеренное — подтвердите."
      action={
        !readOnly && (
          <Popconfirm
            title={`Подтвердить отключение ${hold.removed} учётных записей?`}
            okText="Подтвердить"
            onConfirm={() =>
              mutate(
                { url: `${url}/approve`, method: "post", values: undefined, successNotification: { type: "success", message: "Отключение подтверждено" } },
                { onSuccess: () => void query.refetch() },
              )
            }
          >
            <Button danger loading={mutation.isPending}>
              Подтвердить отключение
            </Button>
          </Popconfirm>
        )
      }
    />
  );
}

/** Организация, остановка синхронизации каталога и политика организации. */
export function OrganizationPage() {
  const { result: org, query } = useCustom<Schemas["Organization"]>({ url: ORG, method: "get" });
  const { mutate, mutation } = useCustomMutation<Schemas["PolicyBody"]>();
  return (
    <Show title="Организация" isLoading={query.isLoading} goBack={false} breadcrumb={false} headerButtons={() => null}>
      <SyncHold />
      <Descriptions column={1}>
        <Descriptions.Item label="Идентификатор">{org.data?.id}</Descriptions.Item>
        <Descriptions.Item label="Организация по умолчанию">{org.data?.default ? "да" : "нет"}</Descriptions.Item>
      </Descriptions>
      <Divider orientation="left">Политика организации</Divider>
      <PolicyForm
        policies={org.data}
        saving={mutation.isPending}
        onSave={(values) =>
          mutate(
            {
              url: `${ORG}/policy`,
              method: "put",
              values,
              successNotification: { type: "success", message: "Политика организации сохранена" },
            },
            { onSuccess: () => void query.refetch() },
          )
        }
      />
    </Show>
  );
}
