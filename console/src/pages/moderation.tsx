import { List } from "@refinedev/antd";

import { ModerationQueue } from "../components/moderation-queue";

/** Очередь модерации организации: письма модерируемых групп и правил потока почты. */
export function ModerationPage() {
  return (
    <List title="Очередь модерации">
      <ModerationQueue />
    </List>
  );
}
