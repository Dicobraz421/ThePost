-- Признак сжатия объекта в учёте BLOB (META-24; SRS 3.2, CONTRACTS.md#BlobIndex): `none` — объект хранится как
-- есть, `zstd` — кадр zstd исходного содержимого; хэш и размер в учёте — от исходного содержимого.
-- Существующие записи получают `none` (все объекты до этой миграции записаны без сжатия). Постоянное значение
-- по умолчанию не переписывает таблицу (PostgreSQL 11+).
ALTER TABLE blobs ADD COLUMN compression text NOT NULL DEFAULT 'none'
    CONSTRAINT blobs_compression CHECK (compression IN ('none', 'zstd'));
