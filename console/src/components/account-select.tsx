// Выбор учётных записей по началу имени входа (поиск на узле); подписи уже выбранных загружаются по идентификаторам.
import { useSelect } from "@refinedev/antd";
import { Select } from "antd";
import { useState } from "react";

import type { Schemas } from "../api/client";

export function AccountSelect(p: { id?: string; value?: string[]; onChange?: (v: string[]) => void; disabled?: boolean }) {
  // Подписи загружаются один раз — для состава на момент открытия формы.
  const [initial] = useState(p.value ?? []);
  const { selectProps } = useSelect<Schemas["AccountInfo"]>({
    resource: "accounts",
    optionLabel: "login",
    optionValue: "id",
    defaultValue: initial,
    pagination: { mode: "off" },
    onSearch: (value) => (value ? [{ field: "prefix", operator: "eq", value }] : []),
  });
  return (
    <Select
      options={selectProps.options}
      loading={selectProps.loading}
      onSearch={selectProps.onSearch}
      filterOption={false}
      id={p.id}
      mode="multiple"
      value={p.value}
      onChange={p.onChange}
      disabled={p.disabled}
      placeholder="Начните вводить имя входа"
    />
  );
}
