-- META-33 (CTR-35, решение 2026-11-04-exchange-parity): личные списки отправителей, почтовые контакты,
-- оргструктура, динамические группы, состояние учётной записи (отключённый ящик со сроком хранения).
-- Данные — в основной БД (каталог организации); все таблицы — с RLS по организации.

-- Списки отправителей ящика (SenderLists): хранимый вид `SenderList::normalized`, `state` — JMAP state.
-- Учётную запись контракт не проверяет: внешнего ключа нет, строки удаляет tp-meta вместе с учётной записью.
CREATE TABLE sender_lists (
    tenant_id  uuid NOT NULL REFERENCES tenants,
    account_id uuid NOT NULL,
    state      bigint NOT NULL CHECK (state > 0),
    safe       text[] NOT NULL,
    blocked    text[] NOT NULL,
    PRIMARY KEY (tenant_id, account_id)
);

-- Почтовые контакты (ContactStore). Адрес уникален в организации без учёта регистра; `groups` — группы
-- рассылки по возрастанию без повторов (группы контракт не проверяет: состав задаёт каталог).
CREATE TABLE mail_contacts (
    tenant_id       uuid NOT NULL REFERENCES tenants,
    contact_id      uuid NOT NULL,
    display_name    text NOT NULL CHECK (length(display_name) BETWEEN 1 AND 255),
    email           text NOT NULL CHECK (octet_length(email) <= 320),
    external_id     text CHECK (external_id <> ''),
    attributes      jsonb NOT NULL DEFAULT '{}',
    groups          uuid[] NOT NULL DEFAULT '{}',
    hidden_from_gal boolean NOT NULL DEFAULT false,
    PRIMARY KEY (tenant_id, contact_id)
);

CREATE UNIQUE INDEX mail_contacts_email ON mail_contacts (tenant_id, lower(email));
CREATE UNIQUE INDEX mail_contacts_external ON mail_contacts (tenant_id, external_id);
CREATE INDEX mail_contacts_groups ON mail_contacts USING gin (groups);

-- Подразделения (OrgStore): дерево, вышестоящее — в той же организации; удаление с вложенными запрещено.
CREATE TABLE org_units (
    tenant_id   uuid NOT NULL REFERENCES tenants,
    unit_id     uuid NOT NULL,
    name        text NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
    parent_id   uuid,
    external_id text CHECK (external_id <> ''),
    PRIMARY KEY (tenant_id, unit_id),
    FOREIGN KEY (tenant_id, parent_id) REFERENCES org_units
);

CREATE UNIQUE INDEX org_units_external ON org_units (tenant_id, external_id);

-- Оргатрибуты учётной записи; строка без заданных полей не хранится (`OrgAttributes::default()`).
CREATE TABLE org_attributes (
    tenant_id     uuid NOT NULL,
    account_id    uuid NOT NULL,
    department_id uuid,
    title         text CHECK (length(title) <= 255),
    manager_id    uuid,
    seniority     integer,
    attributes    jsonb NOT NULL DEFAULT '{}',
    PRIMARY KEY (tenant_id, account_id),
    FOREIGN KEY (tenant_id, account_id) REFERENCES accounts ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, department_id) REFERENCES org_units ON DELETE SET NULL (department_id),
    FOREIGN KEY (tenant_id, manager_id) REFERENCES accounts ON DELETE SET NULL (manager_id),
    CHECK (manager_id IS DISTINCT FROM account_id)
);

CREATE INDEX org_attributes_department ON org_attributes (tenant_id, department_id);

-- Фильтры динамических групп (DynamicGroupStore): группа рассылки — в `groups`.
CREATE TABLE dynamic_groups (
    tenant_id uuid NOT NULL,
    group_id  uuid NOT NULL,
    filter    jsonb NOT NULL,
    contacts  boolean NOT NULL,
    PRIMARY KEY (tenant_id, group_id),
    FOREIGN KEY (tenant_id, group_id) REFERENCES groups ON DELETE CASCADE
);

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['sender_lists', 'mail_contacts', 'org_units', 'org_attributes', 'dynamic_groups']
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I USING (tp_tenant_visible(tenant_id))
                        WITH CHECK (tp_tenant_visible(tenant_id))', t);
    END LOOP;
END
$$;

-- Состояние ящика (AccountStore::account_state): NULL — действует, иначе отключён до срока хранения.
ALTER TABLE accounts ADD COLUMN disabled_until timestamptz;
CREATE INDEX accounts_disabled ON accounts (disabled_until, account_id) WHERE disabled_until IS NOT NULL;

SELECT tp_grant_roles();
