// Выбор учётных записей по началу имени входа (поиск на узле). Подписи уже выбранных — из `names` ответа узла
// (`GroupInfo.names`, ADM-20), без запросов по каждой записи; выбранные при поиске запоминаются здесь же.
import { useSelect } from "@refinedev/antd";
import { Select } from "antd";
import { useState } from "react";

import type { Schemas } from "../api/client";

type Option = { value: string; label: string };

export function AccountSelect(p: {
  id?: string;
  value?: string[];
  onChange?: (v: string[]) => void;
  disabled?: boolean;
  /** Имена входа по идентификатору; идентификатора нет — записи уже нет. */
  names?: Record<string, string>;
}) {
  const [labels, setLabels] = useState<Record<string, string>>(p.names ?? {});
  const { selectProps } = useSelect<Schemas["AccountInfo"]>({
    resource: "accounts",
    optionLabel: "login",
    optionValue: "id",
    pagination: { mode: "off" },
    onSearch: (value) => (value ? [{ field: "prefix", operator: "eq", value }] : []),
  });
  const found = (selectProps.options ?? []) as Option[];
  const chosen = (p.value ?? [])
    .filter((id) => !found.some((o) => o.value === id))
    .map((id) => ({ value: id, label: labels[id] ?? `${id} (записи нет)` }));
  return (
    <Select
      options={[...chosen, ...found]}
      loading={selectProps.loading}
      onSearch={selectProps.onSearch}
      filterOption={false}
      id={p.id}
      mode="multiple"
      value={p.value}
      onChange={(v: string[], options) => {
        const picked = [options].flat().filter((o): o is Option => !!o && "label" in o);
        setLabels((l) => ({ ...l, ...Object.fromEntries(picked.map((o) => [o.value, String(o.label)])) }));
        p.onChange?.(v);
      }}
      disabled={p.disabled}
      placeholder="Начните вводить имя входа"
    />
  );
}
