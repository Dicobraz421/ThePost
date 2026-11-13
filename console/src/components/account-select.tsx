// Выбор учётных записей по началу имени входа (поиск на узле). Подписи уже выбранных — из `names` ответа узла
// (`GroupInfo.names`, ADM-20), без запросов по каждой записи; выбранные при поиске запоминаются здесь же.
import { useSelect } from "@refinedev/antd";
import { Select } from "antd";
import { useState } from "react";

import type { Schemas } from "../api/client";

type Option = { value: string; label: string };
type Props<V> = {
  id?: string;
  value?: V;
  onChange?: (v: V) => void;
  disabled?: boolean;
  /** Имена входа по идентификатору; идентификатора нет — записи уже нет. */
  names?: Record<string, string>;
};

function useAccountOptions(value: string[], names?: Record<string, string>) {
  const [labels, setLabels] = useState<Record<string, string>>(names ?? {});
  const { selectProps } = useSelect<Schemas["AccountInfo"]>({
    resource: "accounts",
    optionLabel: "login",
    optionValue: "id",
    pagination: { mode: "off" },
    onSearch: (v) => (v ? [{ field: "prefix", operator: "eq", value: v }] : []),
  });
  const found = (selectProps.options ?? []) as Option[];
  const chosen = value
    .filter((id) => !found.some((o) => o.value === id))
    .map((id) => ({ value: id, label: labels[id] ?? `${id} (записи нет)` }));
  const remember = (options: unknown) => {
    const picked = [options].flat().filter((o): o is Option => !!o && typeof o === "object" && "label" in o);
    setLabels((l) => ({ ...l, ...Object.fromEntries(picked.map((o) => [o.value, String(o.label)])) }));
  };
  return {
    remember,
    props: {
      options: [...chosen, ...found],
      loading: selectProps.loading,
      onSearch: selectProps.onSearch,
      filterOption: false,
      showSearch: true,
      placeholder: "Начните вводить имя входа",
    },
  };
}

/** Несколько учётных записей (участники, модераторы). */
export function AccountSelect(p: Props<string[]>) {
  const { props, remember } = useAccountOptions(p.value ?? [], p.names);
  return (
    <Select
      {...props}
      id={p.id}
      mode="multiple"
      value={p.value}
      onChange={(v: string[], options) => {
        remember(options);
        p.onChange?.(v);
      }}
      disabled={p.disabled}
    />
  );
}

/** Одна учётная запись. */
export function AccountPick(p: Props<string>) {
  const { props, remember } = useAccountOptions(p.value ? [p.value] : [], p.names);
  return (
    <Select
      {...props}
      id={p.id}
      value={p.value}
      onChange={(v: string, option) => {
        remember(option);
        p.onChange?.(v);
      }}
      disabled={p.disabled}
    />
  );
}
