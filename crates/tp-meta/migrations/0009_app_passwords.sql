-- Пароли приложений (CTR-21; SRS 5.1): хэш (argon2id, PHC-строка), значение не хранится.
-- Отозванные пароли остаются в списке (Admin API показывает историю); удаляются вместе с учётной записью.
CREATE TABLE app_passwords (
    tenant_id   uuid NOT NULL,
    account_id  uuid NOT NULL,
    password_id text NOT NULL CHECK (length(password_id) BETWEEN 1 AND 64
                                     AND password_id !~ '[[:space:][:cntrl:]]'),
    label       text NOT NULL CHECK (length(label) <= 255 AND label !~ '[[:cntrl:]]'),
    hash        text NOT NULL CHECK (hash <> ''),
    created_at  timestamptz NOT NULL,
    revoked_at  timestamptz,
    PRIMARY KEY (tenant_id, account_id, password_id),
    FOREIGN KEY (tenant_id, account_id) REFERENCES accounts ON DELETE CASCADE
);

ALTER TABLE app_passwords ENABLE ROW LEVEL SECURITY;
ALTER TABLE app_passwords FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON app_passwords USING (tp_tenant_visible(tenant_id))
    WITH CHECK (tp_tenant_visible(tenant_id));
