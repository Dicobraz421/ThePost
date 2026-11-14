-- Уведомления о доступе к коллекциям (META-83; CTR-86, CONTRACTS.md#Sharing; JMAP `ShareNotification`, RFC 9670 §5).
-- Каталог (основная БД): общие для узлов, видит только получатель; удаление учётной записи-получателя удаляет её
-- уведомления, состояние и журнал внешними ключами.

-- Состояние уведомлений получателя (JMAP `state`): растёт при каждой записи и удалении; нет строки — 0.
-- Изменения с `state <= floor` удалены из журнала: `since` ниже — `Conflict` (`cannotCalculateChanges`).
CREATE TABLE share_notification_state (
    tenant_id  uuid NOT NULL,
    account_id uuid NOT NULL,
    state      bigint NOT NULL DEFAULT 0 CHECK (state >= 0),
    floor      bigint NOT NULL DEFAULT 0 CHECK (floor BETWEEN 0 AND state),
    PRIMARY KEY (tenant_id, account_id),
    FOREIGN KEY (tenant_id, account_id) REFERENCES accounts ON DELETE CASCADE
);

-- Уведомление (не меняется: только запись и удаление). Права — как выданы, битовая маска: 1 — free_busy,
-- 2 — read, 4 — write, 8 — schedule, 16 — admin (0 — нет прав). Владелец и автор изменения — без внешних
-- ключей: уведомление переживает их удаление.
CREATE TABLE share_notifications (
    tenant_id          uuid NOT NULL,
    account_id         uuid NOT NULL,
    notification_id    uuid NOT NULL,
    created            timestamptz NOT NULL,
    changed_by_name    text NOT NULL,
    changed_by_email   text,
    changed_by_account uuid,
    object_type        text NOT NULL CHECK (object_type IN ('calendar', 'task_list', 'address_book')),
    owner_id           uuid NOT NULL,
    collection_id      uuid NOT NULL,
    old_rights         smallint NOT NULL CHECK (old_rights BETWEEN 0 AND 31),
    new_rights         smallint NOT NULL CHECK (new_rights BETWEEN 0 AND 31),
    name               text NOT NULL,
    CHECK (old_rights <> new_rights),
    PRIMARY KEY (tenant_id, account_id, notification_id),
    FOREIGN KEY (tenant_id, account_id) REFERENCES share_notification_state ON DELETE CASCADE
);

-- Срок хранения (`expire_notifications`): старшие первыми во всех организациях.
CREATE INDEX share_notifications_created ON share_notifications (created, notification_id);

-- Журнал состояний (`notification_changes`): запись (`created`) или удаление уведомления на шаге `state`.
-- `notice_created` — момент изменения прав уведомления: срок хранения убирает из журнала историю удалённых
-- уведомлений старше срока, поднимая `floor`.
CREATE TABLE share_notification_log (
    tenant_id       uuid NOT NULL,
    account_id      uuid NOT NULL,
    state           bigint NOT NULL CHECK (state > 0),
    notification_id uuid NOT NULL,
    created         boolean NOT NULL,
    notice_created  timestamptz NOT NULL,
    PRIMARY KEY (tenant_id, account_id, state),
    FOREIGN KEY (tenant_id, account_id) REFERENCES share_notification_state ON DELETE CASCADE
);

CREATE INDEX share_notification_log_notice_created ON share_notification_log (notice_created);

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['share_notification_state', 'share_notifications', 'share_notification_log']
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I USING (tp_tenant_visible(tenant_id))'
                       ' WITH CHECK (tp_tenant_visible(tenant_id))', t);
    END LOOP;
END
$$;

SELECT tp_grant_roles();
