-- Планирование встреч (META-51; SRS 4.4; CONTRACTS.md#Schedule, CTR-47): индекс занятости объектов, входящие
-- планирования, Schedule-Tag объекта, прозрачность календаря.
--
-- Таблицы ящика: лежат на шарде учётной записи, внешних ключей к каталогу нет (как у dav_objects, META-46).
-- Номера изменений входящих — из счётчика mailbox_state.modseq, общего с письмами и календарями; состояние
-- календарей (dav_calendar_state) входящие не меняют.

ALTER TABLE mailbox_state
    -- ModSeq последнего изменения входящих планирования (запись или удаление); 0 — изменений не было.
    ADD COLUMN dav_inbox_state bigint NOT NULL DEFAULT 0;

ALTER TABLE dav_collections
    -- Календарь не влияет на занятость (RFC 6638 schedule-calendar-transp: transparent).
    ADD COLUMN transparent boolean NOT NULL DEFAULT false;

ALTER TABLE dav_objects
    -- Schedule-Tag объекта планирования (RFC 6638 §3.2) без кавычек; NULL — объект без планирования.
    ADD COLUMN schedule_tag text CHECK (octet_length(schedule_tag) BETWEEN 1 AND 64);

-- Индекс занятости объекта: вхождения с развёрнутыми повторами до горизонта meta.dav.busy_horizon (вычисляет
-- tp-dav при записи объекта). Строки заменяются целиком при записи объекта и уходят вместе с ним.
CREATE TABLE dav_busy (
    tenant_id     uuid NOT NULL,
    account_id    uuid NOT NULL,
    object_id     uuid NOT NULL,
    -- Порядок периода в индексе объекта: индекс читается как записан.
    ord           integer NOT NULL CHECK (ord >= 0),
    -- Коллекция объекта: прозрачность календаря — без обращения к dav_objects.
    collection_id uuid NOT NULL,
    -- Период [busy_start, busy_end) в UTC.
    busy_start    timestamptz NOT NULL,
    busy_end      timestamptz NOT NULL,
    kind          text NOT NULL CHECK (kind IN ('busy', 'tentative', 'unavailable')),
    PRIMARY KEY (tenant_id, account_id, object_id, ord),
    FOREIGN KEY (tenant_id, account_id, object_id) REFERENCES dav_objects ON DELETE CASCADE,
    CHECK (busy_end > busy_start)
);

-- Занятость учётных записей в интервале: кандидаты по концу периода (прошедшие отсекаются индексом).
CREATE INDEX dav_busy_range ON dav_busy (tenant_id, account_id, busy_end, busy_start);

-- Входящие планирования (schedule-inbox, RFC 6638 §2.2): сообщения iTIP учётной записи. UID не уникален
-- (REQUEST, затем CANCEL); в коллекции и их изменения не входят, надгробий нет.
CREATE TABLE dav_inbox (
    tenant_id  uuid NOT NULL,
    account_id uuid NOT NULL,
    item_id    uuid NOT NULL,
    -- Имя ресурса — сегмент URL.
    name       text NOT NULL CHECK (octet_length(name) BETWEEN 1 AND 255),
    -- Метод iTIP (RFC 5546): REQUEST, REPLY, CANCEL, ...
    method     text NOT NULL,
    uid        text NOT NULL CHECK (octet_length(uid) BETWEEN 1 AND 1024),
    -- Адрес отправителя без mailto:.
    sender     text NOT NULL CHECK (octet_length(sender) BETWEEN 1 AND 320),
    body       text NOT NULL,
    etag       text NOT NULL CHECK (octet_length(etag) BETWEEN 1 AND 64),
    -- ModSeq последней записи: порядок поступления.
    modseq     bigint NOT NULL,
    PRIMARY KEY (tenant_id, account_id, item_id)
);

CREATE UNIQUE INDEX dav_inbox_name ON dav_inbox (tenant_id, account_id, name);
CREATE INDEX dav_inbox_modseq ON dav_inbox (tenant_id, account_id, modseq);

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['dav_busy', 'dav_inbox']
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I USING (tp_tenant_visible(tenant_id))'
                       ' WITH CHECK (tp_tenant_visible(tenant_id))', t);
    END LOOP;
END
$$;

SELECT tp_grant_roles();
