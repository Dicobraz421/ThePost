import { useGetIdentity, usePermissions } from "@refinedev/core";
import { useQueryClient } from "@tanstack/react-query";
import { Input, Layout, Space, Typography } from "antd";

import { getOrg, type Schemas, setOrg } from "../api/client";

/** Шапка: кто вошёл, выбор организации (администратор инсталляции); выход — в меню. */
export function Header() {
  const { data: me } = useGetIdentity<Schemas["WhoAmI"]>();
  const { data: roles } = usePermissions<Schemas["AdminRole"][]>({});
  const queries = useQueryClient();
  const choose = (org: string) => {
    if (!org.trim()) return;
    setOrg(org.trim());
    void queries.invalidateQueries();
  };
  return (
    <Layout.Header style={{ background: "transparent", display: "flex", justifyContent: "flex-end", padding: "0 24px" }}>
      <Space size="middle">
        {roles?.includes("installation_admin") ? (
          // Списка организаций в Admin API пока нет: администратор инсталляции вводит UUID.
          <Input.Search
            key={getOrg()}
            aria-label="Организация"
            addonBefore="Организация"
            defaultValue={getOrg()}
            enterButton="Выбрать"
            onSearch={choose}
            style={{ width: 580, verticalAlign: "middle" }}
          />
        ) : (
          <Typography.Text type="secondary">Организация: {getOrg()}</Typography.Text>
        )}
        <Typography.Text strong>{me?.login}</Typography.Text>
      </Space>
    </Layout.Header>
  );
}
