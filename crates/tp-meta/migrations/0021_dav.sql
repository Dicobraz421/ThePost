-- Календари и контакты (META-46; SRS 3.1, 4.4; CONTRACTS.md#Dav): коллекции, объекты, надгробия.
--
-- Таблицы ящика: лежат на шарде учётной записи, внешних ключей к каталогу нет (как у mailboxes, META-30).
-- Тела объектов (iCalendar, vCard) — в PostgreSQL, не в BLOB (SRS 3.1). Номер изменения — из счётчика
-- mailbox_state.modseq, общего с письмами; строка счётчика блокируется до фиксации, поэтому номера видны
-- по порядку.

-- Граница очищенных надгробий хранится по виду (учётная запись) и по коллекции: токен полной синхронизации
-- (состояние вида или коллекции) не ниже своей границы, поэтому очистка одного вида или одной коллекции не
-- делает устаревшими токены других.
ALTER TABLE mailbox_state
    -- Надгробия вида с modseq <= границы очищены: since ниже (кроме 0) — Conflict.
    ADD COLUMN dav_calendar_floor bigint NOT NULL DEFAULT 0,
    ADD COLUMN dav_addressbook_floor bigint NOT NULL DEFAULT 0,
    -- Состояние вида (JMAP state): наибольший ModSeq изменений календарей и адресных книг.
    ADD COLUMN dav_calendar_state bigint NOT NULL DEFAULT 0,
    ADD COLUMN dav_addressbook_state bigint NOT NULL DEFAULT 0;

CREATE TABLE dav_collections (
    tenant_id      uuid NOT NULL,
    account_id     uuid NOT NULL,
    collection_id  uuid NOT NULL,
    kind           text NOT NULL CHECK (kind IN ('calendar', 'address_book')),
    -- Сегмент URL; уникален среди коллекций вида учётной записи.
    name           text NOT NULL CHECK (octet_length(name) BETWEEN 1 AND 255),
    display_name   text NOT NULL,
    description    text,
    color          text,
    sort_order     bigint NOT NULL DEFAULT 0 CHECK (sort_order BETWEEN 0 AND 4294967295),
    timezone       text,
    -- Допустимые компоненты объектов: VEVENT, VTODO, VJOURNAL, VCARD.
    components     text[] NOT NULL CHECK (cardinality(components) > 0),
    -- ModSeq создания, последнего изменения свойств, последнего изменения коллекции или её объектов (CTag).
    created_modseq bigint NOT NULL,
    props_modseq   bigint NOT NULL,
    modseq         bigint NOT NULL,
    -- Число объектов (лимит meta.dav.max_objects без подсчёта строк).
    object_count   integer NOT NULL DEFAULT 0 CHECK (object_count >= 0),
    -- Надгробия объектов коллекции с modseq <= границы очищены: since ниже (кроме 0) — Conflict.
    tombstone_floor bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, account_id, collection_id)
);

CREATE UNIQUE INDEX dav_collections_name ON dav_collections (tenant_id, account_id, kind, name);

CREATE TABLE dav_objects (
    tenant_id      uuid NOT NULL,
    account_id     uuid NOT NULL,
    object_id      uuid NOT NULL,
    collection_id  uuid NOT NULL,
    -- Имя ресурса — сегмент URL.
    name           text NOT NULL CHECK (octet_length(name) BETWEEN 1 AND 255),
    uid            text NOT NULL CHECK (octet_length(uid) BETWEEN 1 AND 1024),
    etag           text NOT NULL CHECK (octet_length(etag) BETWEEN 1 AND 64),
    component      text NOT NULL CHECK (component IN ('VEVENT', 'VTODO', 'VJOURNAL', 'VCARD')),
    -- Интервал с развёрнутыми повторами (UTC): span_start NULL — без дат, span_end NULL — бесконечный.
    span_start     timestamptz,
    span_end       timestamptz,
    body           text NOT NULL,
    created_modseq bigint NOT NULL,
    modseq         bigint NOT NULL,
    PRIMARY KEY (tenant_id, account_id, object_id),
    FOREIGN KEY (tenant_id, account_id, collection_id) REFERENCES dav_collections ON DELETE CASCADE,
    CHECK (span_end IS NULL OR (span_start IS NOT NULL AND span_end >= span_start))
);

CREATE UNIQUE INDEX dav_objects_name ON dav_objects (tenant_id, account_id, collection_id, name);
CREATE UNIQUE INDEX dav_objects_uid ON dav_objects (tenant_id, account_id, collection_id, uid);
-- Запрос time-range: кандидаты по началу интервала (без расширений СУБД, SRS 3.1).
CREATE INDEX dav_objects_span ON dav_objects (tenant_id, account_id, collection_id, span_start, span_end);
-- Изменения коллекции (sync-collection) и учётной записи (JMAP /changes).
CREATE INDEX dav_objects_collection_modseq ON dav_objects (tenant_id, account_id, collection_id, modseq);
CREATE INDEX dav_objects_modseq ON dav_objects (tenant_id, account_id, modseq);

-- Надгробия удалённых объектов и коллекций — до очистки старше meta.dav.tombstone_ttl.
CREATE TABLE dav_tombstones (
    tenant_id     uuid NOT NULL,
    account_id    uuid NOT NULL,
    -- У каждого удаления свой ModSeq.
    modseq        bigint NOT NULL,
    kind          text NOT NULL CHECK (kind IN ('calendar', 'address_book')),
    collection_id uuid NOT NULL,
    -- Удалённый объект; NULL — надгробие самой коллекции.
    object_id     uuid,
    -- Имя ресурса или коллекции.
    name          text NOT NULL,
    deleted_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, account_id, modseq)
);

CREATE INDEX dav_tombstones_collection ON dav_tombstones (tenant_id, account_id, collection_id, modseq);
CREATE INDEX dav_tombstones_deleted ON dav_tombstones (deleted_at);

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['dav_collections', 'dav_objects', 'dav_tombstones']
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I USING (tp_tenant_visible(tenant_id))'
                       ' WITH CHECK (tp_tenant_visible(tenant_id))', t);
    END LOOP;
END
$$;

SELECT tp_grant_roles();
