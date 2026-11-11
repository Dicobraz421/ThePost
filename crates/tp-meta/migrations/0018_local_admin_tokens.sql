-- Локальные одноразовые токены Admin API (META-43; CTR-41, CONTRACTS.md#Auth, решение #193): `thepost admin init`
-- выпускает токен администратору организации; хранится только хэш значения (hex SHA-256), значение не хранится.
-- Идентификатор и хэш уникальны по инсталляции: Bearer не несёт организацию, погашение (`consume`), отзыв и очистка
-- идут системной транзакцией (app.all_tenants), выпуск и список — в транзакции организации (RLS).
-- Без внешнего ключа на учётную запись: токен — служебная запись с коротким сроком, удаляется очисткой (`prune`).
CREATE TABLE local_admin_tokens (
    token_id   text PRIMARY KEY CHECK (length(token_id) BETWEEN 1 AND 64
                                       AND token_id !~ '[[:space:][:cntrl:]]'),
    tenant_id  uuid NOT NULL REFERENCES tenants ON DELETE CASCADE,
    account_id uuid NOT NULL,
    hash       text NOT NULL UNIQUE CHECK (hash ~ '^[0-9a-f]{64}$'),
    created_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    used_at    timestamptz,
    revoked_at timestamptz,
    CHECK (expires_at > created_at)
);

-- `consume` — по уникальному индексу хэша; `list` — организация по выпуску; `prune` — по сроку.
CREATE INDEX local_admin_tokens_tenant ON local_admin_tokens (tenant_id, created_at, token_id COLLATE "C");
CREATE INDEX local_admin_tokens_expires ON local_admin_tokens (expires_at);

ALTER TABLE local_admin_tokens ENABLE ROW LEVEL SECURITY;
ALTER TABLE local_admin_tokens FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON local_admin_tokens
    USING (tp_tenant_visible(tenant_id)) WITH CHECK (tp_tenant_visible(tenant_id));

SELECT tp_grant_roles();
