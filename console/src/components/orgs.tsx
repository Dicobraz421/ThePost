// Организации, доступные вошедшему (`GET /api/v1/orgs`, ADM-26): выбор в шапке и имена в списках.
import { useCustom } from "@refinedev/core";
import { useQueryClient } from "@tanstack/react-query";
import { Select } from "antd";
import { useState } from "react";

import { API, getOrg, type Schemas, setOrg } from "../api/client";

type Organization = Schemas["Organization"];

/** Имя организации: собственных имён в Admin API нет — организация по умолчанию и идентификатор остальных. */
export const orgName = (o: Pick<Organization, "id" | "default">) => (o.default ? "по умолчанию" : o.id);

/**
 * Список организаций (один запрос на все экраны) и имя по идентификатору; неизвестная — идентификатор.
 * `enabled: false` — без запроса (аудитору список не положен: узел ответит 403).
 */
export function useOrgs(enabled = true) {
  const { result } = useCustom<Organization[]>({ url: `${API}/orgs`, method: "get", queryOptions: { enabled } });
  // До ответа Refine отдаёт пустой объект, а не список.
  const orgs = Array.isArray(result.data) ? result.data : [];
  const name = (id: string) => {
    const o = orgs.find((o) => o.id === id);
    return o ? orgName(o) : id;
  };
  return { orgs, name };
}

/** Выбор организации из списка: запросы экранов перечитываются уже для неё. */
export function OrgSelect() {
  const { orgs } = useOrgs();
  const queries = useQueryClient();
  const [org, setCurrent] = useState(getOrg());
  return (
    <Select
      aria-label="Организация"
      value={org}
      options={orgs.map((o) => ({ value: o.id, label: orgName(o) }))}
      onChange={(id: string) => {
        setOrg(id);
        setCurrent(id);
        void queries.invalidateQueries();
      }}
      style={{ minWidth: 320 }}
    />
  );
}
