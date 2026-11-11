-- Побайтный порядок списка учётных записей (META-45; CTR-44, CONTRACTS.md#Metadata; решение 21
-- 2026-11-11-stage2-followups): `AccountStore::list` упорядочивает имена входа в нижнем регистре по байтам (COLLATE "C"),
-- как хранилище в памяти, а не по правилам сортировки базы (там `.`, `-`, `_` почти не влияют на порядок).
-- Курсор `after` и префикс (диапазон `>= prefix AND < следующий`) идут по этому индексу.
-- Обычный CREATE INDEX: миграции ядра выполняются в транзакции (CONCURRENTLY нельзя), таблица учётных записей мала.
-- Уникальность имён по-прежнему держит accounts_login (tenant_id, lower(login)).
CREATE INDEX accounts_login_c ON accounts (tenant_id, (lower(login) COLLATE "C"));
