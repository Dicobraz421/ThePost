-- Контракты contracts-38 (CTR-53, CTR-54, CTR-56; CONTRACTS.md#Metadata, CONTRACTS.md#Dav): псевдонимы учётной
-- записи, свойства коллекций (по умолчанию, подписка, видимость, оповещения по умолчанию), поисковые строки контакта
-- и перенос объекта между коллекциями.

ALTER TABLE accounts
    -- Псевдонимы каталога с целью — этой учётной записью: нижний регистр, без повторов, основного адреса и catch-all.
    ADD COLUMN aliases text[] NOT NULL DEFAULT '{}';

ALTER TABLE dav_collections
    -- Коллекция по умолчанию своего вида: не больше одной на вид у учётной записи (индекс ниже).
    ADD COLUMN is_default boolean NOT NULL DEFAULT false,
    -- Подписка и видимость (JMAP isSubscribed, isVisible).
    ADD COLUMN subscribed boolean NOT NULL DEFAULT true,
    ADD COLUMN visible boolean NOT NULL DEFAULT true,
    -- Оповещения по умолчанию календаря: текст JSON-объекта Id[Alert] JSCalendar; NULL — нет.
    ADD COLUMN alerts_with_time text CHECK (octet_length(alerts_with_time) <= 8192),
    ADD COLUMN alerts_without_time text CHECK (octet_length(alerts_without_time) <= 8192);

CREATE UNIQUE INDEX dav_collections_default ON dav_collections (tenant_id, account_id, kind) WHERE is_default;

ALTER TABLE dav_objects
    -- Поисковые строки контакта (ObjectIndex::search): нижний регистр; у объектов календаря — пусто. Поиск — по
    -- вхождению подстроки среди объектов одной коллекции (без расширений СУБД, SRS 3.1).
    ADD COLUMN search text[] NOT NULL DEFAULT '{}' CHECK (cardinality(search) <= 64),
    -- ModSeq появления в нынешней коллекции: создание или перенос (created_modseq перенос не меняет).
    ADD COLUMN placed_modseq bigint;

UPDATE dav_objects SET placed_modseq = created_modseq;

ALTER TABLE dav_objects ALTER COLUMN placed_modseq SET NOT NULL;
