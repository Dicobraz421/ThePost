-- Скрипты Sieve и автоответ (META-49; SRS 3.1, 4.3; CONTRACTS.md#Sieve).
--
-- Скрипты и автоответ учётной записи — таблицы ящика на её шарде, без внешних ключей к каталогу (META-30).
-- Скрипт организации — в основной БД рядом с каталогом организаций. Текст скрипта хранилище не разбирает:
-- его проверяет tp-sieve до записи.

ALTER TABLE mailbox_state
    -- Состояние скриптов и автоответа (JMAP state, версия кэша программ tp-sieve): растёт при каждом изменении.
    ADD COLUMN sieve_state bigint NOT NULL DEFAULT 0;

CREATE TABLE sieve_scripts (
    tenant_id  uuid NOT NULL,
    account_id uuid NOT NULL,
    -- Имя (RFC 5804 §1.6), до 128 символов.
    name       text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 128),
    script     text NOT NULL,
    active     boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, account_id, name)
);

-- Активный скрипт учётной записи — не больше одного.
CREATE UNIQUE INDEX sieve_scripts_active ON sieve_scripts (tenant_id, account_id) WHERE active;

-- Скрипт организации: выполняется до скрипта пользователя у всех учётных записей организации.
CREATE TABLE sieve_org_scripts (
    tenant_id  uuid PRIMARY KEY REFERENCES tenants ON DELETE CASCADE,
    script     text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Автоответ учётной записи (модель JMAP VacationResponse, RFC 8621 §8); нет строки — выключен.
CREATE TABLE vacation (
    tenant_id  uuid NOT NULL,
    account_id uuid NOT NULL,
    enabled    boolean NOT NULL,
    from_at    timestamptz,
    to_at      timestamptz,
    subject    text,
    text_body  text,
    html_body  text,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, account_id),
    CHECK (from_at IS NULL OR to_at IS NULL OR from_at <= to_at)
);

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['sieve_scripts', 'sieve_org_scripts', 'vacation']
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I USING (tp_tenant_visible(tenant_id))'
                       ' WITH CHECK (tp_tenant_visible(tenant_id))', t);
    END LOOP;
END
$$;

SELECT tp_grant_roles();
