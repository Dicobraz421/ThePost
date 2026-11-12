// Роли администраторов (SRS 5.2) в консоли: разделы меню и кнопки по ролям из `whoami`. Права проверяет узел —
// консоль лишь не показывает то, на что узел ответит отказом.
import { type AccessControlProvider, useCan } from "@refinedev/core";

import { type Schemas, whoami } from "../api/client";

type Role = Schemas["AdminRole"];

const WRITE = ["create", "edit", "delete", "clone"];

/** Роли допускают действие с разделом: аудитор — только журнал аудита, оператор — чтение без журнала аудита. */
export function allowed(roles: Role[], resource: string | undefined, action: string) {
  if (!resource || roles.includes("installation_admin")) return true;
  if (resource === "audit") return roles.includes("organization_admin") || roles.includes("auditor");
  return roles.includes("organization_admin") || (roles.includes("operator") && !WRITE.includes(action));
}

export const accessControlProvider: AccessControlProvider = {
  can: async ({ resource, action }) => {
    try {
      return { can: allowed((await whoami()).roles, resource, action) };
    } catch {
      return { can: true }; // роли неизвестны — решает узел
    }
  },
  options: { buttons: { enableAccessControl: true, hideIfUnauthorized: true } },
};

/** Раздел только для чтения (оператор): кнопки изменения не показываются. */
export function useReadOnly(resource: string) {
  return useCan({ resource, action: "edit" }).data?.can === false;
}
