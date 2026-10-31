-- Манифесты поисковых индексов (CTR-09, META-12; SRS 4.6).
-- Одна строка на учётную запись: версия (CAS put_manifest; растёт и при запросе перестройки), схема индекса,
-- сегменты (jsonb: [{path, size, docs}]) и запрошенная перестройка. Нет строки — индекса нет (версия 0).
CREATE TABLE index_manifests (
    tenant_id            uuid NOT NULL,
    account_id           uuid NOT NULL,
    version              bigint NOT NULL CHECK (version > 0),
    schema_version       bigint NOT NULL CHECK (schema_version BETWEEN 0 AND 4294967295),
    segments             jsonb NOT NULL DEFAULT '[]',
    rebuild_requested_at timestamptz,
    rebuild_reason       text CHECK (rebuild_reason IN ('admin', 'corrupt', 'restore')),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, account_id),
    FOREIGN KEY (tenant_id, account_id) REFERENCES accounts ON DELETE CASCADE,
    CHECK ((rebuild_requested_at IS NULL) = (rebuild_reason IS NULL))
);

-- pending_rebuilds: запрошенные перестройки и индексы старой схемы (по всем организациям).
CREATE INDEX index_manifests_rebuild ON index_manifests (rebuild_requested_at) WHERE rebuild_requested_at IS NOT NULL;
CREATE INDEX index_manifests_schema ON index_manifests (schema_version);

ALTER TABLE index_manifests ENABLE ROW LEVEL SECURITY;
ALTER TABLE index_manifests FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON index_manifests USING (tp_tenant_visible(tenant_id))
    WITH CHECK (tp_tenant_visible(tenant_id));
