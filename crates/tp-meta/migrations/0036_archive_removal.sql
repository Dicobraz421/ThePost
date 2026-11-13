-- Архив ящика и отметка удаления отключённого ящика (CTR-76, CTR-78; CONTRACTS.md#Archive, #Metadata).

-- Политика архива уровня цепочки (организация → домен → ящик): возраст автопереноса (микросекунды, не больше
-- 100 лет — срок письма received_at + возраст остаётся в диапазоне timestamptz) и квота архива. NULL — не задано.
ALTER TABLE policies
    ADD COLUMN archive_after_us bigint CHECK (archive_after_us BETWEEN 0 AND 3155760000000000),
    ADD COLUMN archive_quota_bytes bigint CHECK (archive_quota_bytes >= 0);

-- Окончательное удаление отключённого ящика начато (AccountStore::begin_removal): восстановление, подключение и
-- перенос срока — Conflict. Только у отключённого (disabled_until задан).
ALTER TABLE accounts
    ADD COLUMN removing boolean NOT NULL DEFAULT false,
    ADD CONSTRAINT accounts_removing_disabled CHECK (NOT removing OR disabled_until IS NOT NULL);
