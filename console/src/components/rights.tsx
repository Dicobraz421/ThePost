// Права на календари и адресные книги (ADM-16, CTR-48): старшее включает младшие (`admin` — все, `write` — `read`,
// `read` — `free_busy`), `schedule` — отдельное. Узел хранит права как выданы; здесь — выбор и подпись.
import { Checkbox, Tag } from "antd";

import type { Schemas } from "../api/client";

export type Rights = Schemas["RightsBody"];
export type Right = keyof Rights;

export const RIGHTS: Record<Right, string> = {
  free_busy: "занятость",
  read: "чтение",
  write: "запись",
  schedule: "приглашения от имени",
  admin: "управление",
};

export const NO_RIGHTS: Rights = { free_busy: false, read: false, write: false, schedule: false, admin: false };

const ORDER = Object.keys(RIGHTS) as Right[];

/** Права выбранными флажками (`Form.Item` с массивом имён) и обратно. */
export const toRights = (names: Right[] = []): Rights => ({ ...NO_RIGHTS, ...Object.fromEntries(names.map((n) => [n, true])) });
export const fromRights = (r?: Rights | null): Right[] => ORDER.filter((n) => r?.[n]);

/** Флажки прав (значение — массив имён прав). */
export function RightsChecks(p: { value?: Right[]; onChange?: (v: Right[]) => void; disabled?: boolean }) {
  return (
    <Checkbox.Group
      value={p.value}
      onChange={(v) => p.onChange?.(v as Right[])}
      disabled={p.disabled}
      options={ORDER.map((value) => ({ value, label: RIGHTS[value] }))}
    />
  );
}

/** Права метками; нет прав — «нет». */
export function RightsTags({ rights }: { rights?: Rights | null }) {
  const names = fromRights(rights);
  return names.length ? (
    <>
      {names.map((n) => (
        <Tag key={n}>{RIGHTS[n]}</Tag>
      ))}
    </>
  ) : (
    <Tag>нет</Tag>
  );
}
