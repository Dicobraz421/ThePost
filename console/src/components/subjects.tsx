// Тема и отправитель писем в списках корзины и очереди модерации (UI-15, ADM-19): только по явному запросу
// администратора (`include=subject`) — узел пишет такой запрос в журнал аудита как доступ к содержимому писем.
import { Space, Switch, Typography } from "antd";
import { useState } from "react";

import { useReadOnly } from "../providers/access";

/** `include` запроса списка и переключатель; оператору (только чтение) темы недоступны — переключателя нет. */
export function useSubjects(resource: string) {
  const readOnly = useReadOnly(resource);
  const [on, setOn] = useState(false);
  const shown = on && !readOnly;
  const toggle = readOnly ? null : (
    <Space style={{ marginBottom: 16 }}>
      <Switch aria-label="Показать темы писем" checked={on} onChange={setOn} />
      <span>Показать темы писем</span>
      <Typography.Text type="secondary">запрос записывается в журнал аудита как доступ к содержимому</Typography.Text>
    </Space>
  );
  return { include: shown ? "subject" : undefined, shown, toggle };
}
