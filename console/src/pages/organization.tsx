import { Show } from "@refinedev/antd";
import { useCustom, useCustomMutation } from "@refinedev/core";
import { Descriptions, Divider } from "antd";

import type { Schemas } from "../api/client";
import { PolicyForm } from "../components/policy-form";

const ORG = "/api/v1/orgs/{org}";

/** Организация и её политика. */
export function OrganizationPage() {
  const { result: org, query } = useCustom<Schemas["Organization"]>({ url: ORG, method: "get" });
  const { mutate, mutation } = useCustomMutation<Schemas["PolicyBody"]>();
  return (
    <Show title="Организация" isLoading={query.isLoading} goBack={false} breadcrumb={false} headerButtons={() => null}>
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
