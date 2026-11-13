// Выбор групп рассылки организации (по имени): кому разрешено бронировать ресурс, группы политики общего доступа.
import { useList } from "@refinedev/core";
import { Select } from "antd";

import type { Schemas } from "../api/client";

export function GroupSelect(p: { id?: string; value?: string[]; onChange?: (v: string[]) => void; disabled?: boolean }) {
  const { result, query } = useList<Schemas["GroupInfo"]>({ resource: "groups", pagination: { mode: "off" } });
  const known = result.data ?? [];
  const missing = (p.value ?? []).filter((id) => !known.some((g) => g.id === id));
  const options = [
    ...known.map((g) => ({ value: g.id, label: g.name })),
    ...missing.map((id) => ({ value: id, label: `${id} (группы нет)` })),
  ];
  return (
    <Select
      id={p.id}
      mode="multiple"
      value={p.value}
      onChange={p.onChange}
      disabled={p.disabled}
      loading={query.isLoading}
      options={options}
      optionFilterProp="label"
      placeholder="Группы"
    />
  );
}
