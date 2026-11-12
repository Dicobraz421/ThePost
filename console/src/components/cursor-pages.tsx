// Страницы журналов Admin API: ответ — объект с `next` (значение `after` следующей страницы).
import { useCustom } from "@refinedev/core";
import { Button, Space } from "antd";
import { useState } from "react";

type Cursor = string | number | undefined;

/** Страница по условиям `query`; смена условий возвращает к первой странице. */
export function useCursorPages<T extends { next?: string | number | null }>(url: string, query: Record<string, unknown>) {
  const key = JSON.stringify(query);
  // Курсоры пройденных страниц: последний — текущая.
  const [state, setState] = useState<{ key: string; cursors: Cursor[] }>({ key, cursors: [undefined] });
  const cursors = state.key === key ? state.cursors : [undefined];
  const after = cursors[cursors.length - 1];
  const { result, query: q } = useCustom<T>({ url, method: "get", config: { query: { ...query, after } } });
  const next = result.data?.next;
  const pager = (
    <Space style={{ marginTop: 16 }}>
      <Button disabled={cursors.length < 2} onClick={() => setState({ key, cursors: cursors.slice(0, -1) })}>
        Назад
      </Button>
      <Button disabled={next == null} onClick={() => next != null && setState({ key, cursors: [...cursors, next] })}>
        Дальше
      </Button>
    </Space>
  );
  return { page: result.data, loading: q.isLoading, pager };
}
