-- Хранилища ящика на шардах (META-30; SRS 3.6, решение 2026-11-01-scale-shards-geo-admin).
--
-- Схема ядра применяется на каждом шарде справочника; данные каталога (организации, домены, учётные записи,
-- справочник шардов) — только в основной БД (шард 0), на других шардах эти таблицы пусты. Транзакции ящика
-- не обращаются к каталогу: счётчики ящика (ModSeq, нижняя граница журнала изменений, последний UIDVALIDITY)
-- переходят из accounts в mailbox_state на шарде ящика; внешние ключи таблиц ящика к каталогу удаляются
-- (учётные записи не удаляются, каскад не использовался).

-- Площадка шарда (геораспределение) и адрес реплики для фоновых чтений (`ReadIntent::Stale`, META-32).
ALTER TABLE shards
    ADD COLUMN site text CONSTRAINT shards_site_name CHECK (
        length(site) BETWEEN 1 AND 255 AND site !~ '[[:space:][:cntrl:]]'
    ),
    ADD COLUMN read_replica text CONSTRAINT shards_read_replica_without_password CHECK (
        read_replica !~ '^[A-Za-z][A-Za-z0-9+.-]*://[^/?#@]*:[^/?#@]*@'
        AND read_replica !~* '[?&]password='
    );

CREATE TABLE mailbox_state (
    tenant_id         uuid NOT NULL,
    account_id        uuid NOT NULL,
    -- Счётчик ModSeq учётной записи (CONTRACTS.md#Metadata).
    modseq            bigint NOT NULL DEFAULT 0,
    -- Изменения с modseq <= changelog_floor удалены из журнала: since ниже — Conflict.
    changelog_floor   bigint NOT NULL DEFAULT 0,
    -- Последний выданный UIDVALIDITY папок (строго растёт).
    uid_validity_last bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, account_id)
);

ALTER TABLE mailbox_state ENABLE ROW LEVEL SECURITY;
ALTER TABLE mailbox_state FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON mailbox_state USING (tp_tenant_visible(tenant_id))
    WITH CHECK (tp_tenant_visible(tenant_id));

-- Перенос счётчиков: транзакция миграции видит строки всех организаций.
SELECT set_config('app.all_tenants', 'on', true);
INSERT INTO mailbox_state (tenant_id, account_id, modseq, changelog_floor, uid_validity_last)
SELECT tenant_id, account_id, modseq, changelog_floor, uid_validity_last FROM accounts WHERE shard_id = 0;

ALTER TABLE accounts DROP COLUMN modseq, DROP COLUMN changelog_floor, DROP COLUMN uid_validity_last;

-- Учётная запись основной БД получает состояние ящика при создании; на другом шарде его создаёт
-- tp-meta вместе с системными папками.
CREATE FUNCTION tp_mailbox_state_init() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO mailbox_state (tenant_id, account_id) VALUES (NEW.tenant_id, NEW.account_id)
    ON CONFLICT DO NOTHING;
    RETURN NULL;
END
$$;

CREATE TRIGGER tp_mailbox_state_init AFTER INSERT ON accounts
    FOR EACH ROW WHEN (NEW.shard_id = 0) EXECUTE FUNCTION tp_mailbox_state_init();

DO $$
DECLARE
    c record;
BEGIN
    FOR c IN
        SELECT conrelid::regclass AS tbl, conname FROM pg_constraint
        WHERE contype = 'f'
          AND conrelid IN ('index_manifests'::regclass, 'send_delegations'::regclass, 'queue'::regclass)
          AND confrelid IN ('accounts'::regclass, 'tenants'::regclass)
    LOOP
        EXECUTE format('ALTER TABLE %s DROP CONSTRAINT %I', c.tbl, c.conname);
    END LOOP;
END
$$;

SELECT tp_grant_roles();
