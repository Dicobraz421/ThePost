-- Роль администратора установки — явная запись (решение 2026-11-11-stage2-followups, п. 117; ADM-41): её ставит
-- `thepost admin init` (`AccountStore::grant_installation_admin`), читает вход администратора по паролю
-- (`AccountStore::is_installation_admin`). Запись удаляется вместе с учётной записью.
CREATE TABLE installation_admins (
    tenant_id  uuid NOT NULL,
    account_id uuid NOT NULL,
    granted_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, account_id),
    FOREIGN KEY (tenant_id, account_id) REFERENCES accounts ON DELETE CASCADE
);

ALTER TABLE installation_admins ENABLE ROW LEVEL SECURITY;
ALTER TABLE installation_admins FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON installation_admins USING (tp_tenant_visible(tenant_id))
    WITH CHECK (tp_tenant_visible(tenant_id));

-- Перенос роли: до этой миграции администратора установки узнавали по действующему паролю приложения с подписью
-- `thepost admin init` в организации по умолчанию — признак читается здесь один раз, дальше роль — только запись.
-- Транзакция миграции видит строки всех организаций; данные не удаляются.
SELECT set_config('app.all_tenants', 'on', true);
INSERT INTO installation_admins (tenant_id, account_id)
SELECT DISTINCT tenant_id, account_id FROM app_passwords
WHERE tenant_id = '00000000-0000-0000-0000-000000000001' AND label = 'thepost admin init' AND revoked_at IS NULL;
SELECT set_config('app.all_tenants', '', true);

SELECT tp_grant_roles();
