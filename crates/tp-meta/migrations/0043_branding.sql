-- Брендинг заказчика (META-84; CTR-91, CONTRACTS.md#Branding; решение 122). Каталог (основная БД): по строке на
-- уровень — установка (`tenant_id IS NULL`, не больше одной строки) и организация (`tenant_id` уникален). Все поля
-- необязательны: незаданное берётся с уровня выше (организация → установка → IT TASK, `BrandingLevels::effective`).
-- Заданные роли — `roles`: `{"light": {...}, "dark": {...}, "cli": {...}}`, имена ролей и цвета проверяет контракт.
-- Байты знака — в BLOB по пути `branding/<hash>.png` (`BrandLogo::path`), здесь — только описание; удаление
-- организации удаляет её набор.
CREATE TABLE branding (
    tenant_id     uuid UNIQUE REFERENCES tenants ON DELETE CASCADE,
    product_name  text CHECK (char_length(product_name) BETWEEN 1 AND 64),
    primary_color text CHECK (primary_color ~ '^#[0-9A-F]{6}$'),
    roles         jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(roles) = 'object'),
    logo_hash     bytea CHECK (octet_length(logo_hash) = 32),
    logo_size     bigint CHECK (logo_size BETWEEN 1 AND 262144),
    logo_width    integer CHECK (logo_width BETWEEN 1 AND 1024),
    logo_height   integer CHECK (logo_height BETWEEN 1 AND 1024),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    -- Знак задан целиком или не задан.
    CHECK ((logo_hash IS NULL) = (logo_size IS NULL)
       AND (logo_hash IS NULL) = (logo_width IS NULL)
       AND (logo_hash IS NULL) = (logo_height IS NULL))
);

-- Уровень установки — не больше одной строки (цель `ON CONFLICT` записи набора установки).
CREATE UNIQUE INDEX branding_installation ON branding ((tenant_id IS NULL)) WHERE tenant_id IS NULL;

-- `logo_in_use`: упомянут ли знак хоть одним уровнем.
CREATE INDEX branding_logo ON branding (logo_hash) WHERE logo_hash IS NOT NULL;

-- Набор организации видит и меняет только она; набор установки читают все (он публичен: экран входа без входа),
-- меняет — только системная транзакция (`app.all_tenants`): у `tenant_id IS NULL` проверка `tp_tenant_visible`
-- ложна.
ALTER TABLE branding ENABLE ROW LEVEL SECURITY;
ALTER TABLE branding FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON branding USING (tp_tenant_visible(tenant_id))
    WITH CHECK (tp_tenant_visible(tenant_id));
CREATE POLICY installation_read ON branding FOR SELECT USING (tenant_id IS NULL);

SELECT tp_grant_roles();
