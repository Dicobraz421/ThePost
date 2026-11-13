-- Журнал входящих планирования (META-69; CONTRACTS.md#Dav, CTR-65; JMAP CalendarEventNotification/changes): время
-- поступления и ModSeq создания ресурса, надгробия удалённых ресурсов и граница очищенных.
--
-- Таблица ящика: лежит на шарде учётной записи, как dav_inbox (META-51). Номера надгробий — из счётчика
-- mailbox_state.modseq; надгробия старше meta.dav.tombstone_ttl очищает purge_dav_tombstones вместе с надгробиями
-- DAV и поднимает mailbox_state.dav_inbox_floor.

ALTER TABLE mailbox_state
    -- Надгробия входящих с modseq <= границы очищены: since ниже (кроме 0) — Conflict.
    ADD COLUMN dav_inbox_floor bigint NOT NULL DEFAULT 0;

ALTER TABLE dav_inbox
    -- Время первой записи ресурса (InboxItem::received_at): перезапись его не меняет. У ресурсов до миграции —
    -- время миграции.
    ADD COLUMN received_at timestamptz NOT NULL DEFAULT now(),
    -- ModSeq первой записи: created или updated в inbox_changes.
    ADD COLUMN created_modseq bigint;

UPDATE dav_inbox SET created_modseq = modseq;

ALTER TABLE dav_inbox ALTER COLUMN created_modseq SET NOT NULL;

CREATE TABLE dav_inbox_tombstones (
    tenant_id  uuid NOT NULL,
    account_id uuid NOT NULL,
    -- У каждого удаления свой ModSeq.
    modseq     bigint NOT NULL,
    item_id    uuid NOT NULL,
    deleted_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, account_id, modseq)
);

CREATE INDEX dav_inbox_tombstones_deleted ON dav_inbox_tombstones (deleted_at);

ALTER TABLE dav_inbox_tombstones ENABLE ROW LEVEL SECURITY;
ALTER TABLE dav_inbox_tombstones FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON dav_inbox_tombstones USING (tp_tenant_visible(tenant_id))
    WITH CHECK (tp_tenant_visible(tenant_id));

SELECT tp_grant_roles();
