-- Настройки транспорта (META-28; CTR-24, CONTRACTS.md#Transport; SRS 4.1, 2.6; решение 2026-11-01-transport-scope-external-mta).
-- Уровень кластера — принятые домены (таблица domains) и доверенные источники; версия снимка — одна строка
-- transport_cluster, запись блокирует её (FOR UPDATE), чтение снимка — FOR SHARE. Уровень организации — политики
-- доменов назначения и версия снимка организации (RLS).

-- Вид принятого домена: полномочный или внутренняя ретрансляция (неизвестные адреса — на next_hop).
-- Прежние домены — полномочные.
ALTER TABLE domains
    ADD COLUMN kind text NOT NULL DEFAULT 'authoritative' CHECK (kind IN ('authoritative', 'internal_relay')),
    ADD COLUMN next_hop text CHECK (length(next_hop) BETWEEN 1 AND 255 AND next_hop !~ '[[:space:][:cntrl:]]'),
    ADD CONSTRAINT domains_next_hop CHECK ((kind = 'internal_relay') = (next_hop IS NOT NULL));

-- Версия снимка уровня кластера (одна строка).
CREATE TABLE transport_cluster (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    version   bigint NOT NULL CHECK (version >= 0)
);

INSERT INTO transport_cluster (singleton, version) VALUES (true, 0);

-- Доверенные источники приёма: tenant_id IS NULL — источник кластера (общий шлюз), виден всем; источник
-- организации виден только ей; источник кластера пишет только системная транзакция (app.all_tenants).
CREATE TABLE transport_sources (
    name       text PRIMARY KEY CHECK (name ~ '^[a-z0-9._-]{1,64}$'),
    tenant_id  uuid REFERENCES tenants,
    matcher    jsonb NOT NULL,
    kind       jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX transport_sources_tenant ON transport_sources (tenant_id) WHERE tenant_id IS NOT NULL;

ALTER TABLE transport_sources ENABLE ROW LEVEL SECURITY;
ALTER TABLE transport_sources FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON transport_sources
    USING (tenant_id IS NULL OR tp_tenant_visible(tenant_id))
    WITH CHECK (coalesce(tp_tenant_visible(tenant_id), false));

-- Версия снимка организации; нет строки — версия 0.
CREATE TABLE transport_versions (
    tenant_id uuid PRIMARY KEY REFERENCES tenants,
    version   bigint NOT NULL CHECK (version > 0)
);

-- Политики доменов назначения организации: шаблон — ключ (`example.org`, `*.example.org`, `*`).
CREATE TABLE transport_routes (
    tenant_id  uuid NOT NULL REFERENCES tenants,
    pattern    text NOT NULL CHECK (length(pattern) BETWEEN 1 AND 255),
    tls        jsonb NOT NULL,
    smart_host text CHECK (length(smart_host) BETWEEN 1 AND 255 AND smart_host !~ '[[:space:][:cntrl:]]'),
    egress_ip  inet,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, pattern)
);

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['transport_versions', 'transport_routes']
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I USING (tp_tenant_visible(tenant_id))'
                       ' WITH CHECK (tp_tenant_visible(tenant_id))', t);
    END LOOP;
END
$$;

SELECT tp_grant_roles();
