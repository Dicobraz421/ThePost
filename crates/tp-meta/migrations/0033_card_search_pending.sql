-- META-78: признак «поисковые строки контакта не посчитаны» (ObjectIndex::search, CTR-54; миграция 0030). У
-- карточек, записанных до 0030, строк нет — их пересчитывает PgMeta::reindex_cards. Значение по умолчанию true
-- получают все существующие строки без перезаписи таблицы, затем по умолчанию — false: новые объекты пишутся с
-- посчитанными строками. Признак значим только у VCARD.
ALTER TABLE dav_objects ADD COLUMN search_pending boolean NOT NULL DEFAULT true;
ALTER TABLE dav_objects ALTER COLUMN search_pending SET DEFAULT false;

-- Непосчитанные карточки: после пересчёта индекс пуст, повторный запуск — один пустой просмотр на шард.
CREATE INDEX dav_objects_search_pending ON dav_objects (tenant_id, account_id, object_id)
    WHERE search_pending AND component = 'VCARD';
