-- Таблица переписывания адресов конверта организации (META-39; CTR-40, CONTRACTS.md#Transport; модель Postfix
-- canonical_maps). Ключ — исходный шаблон в канонической форме (`old@example.org`, `@old.example`); версия —
-- общая с политиками доменов назначения (transport_versions). Изоляция организаций — RLS.
CREATE TABLE transport_rewrites (
    tenant_id   uuid NOT NULL REFERENCES tenants,
    from_addr   text NOT NULL CHECK (length(from_addr) BETWEEN 2 AND 320 AND from_addr ~ '@'),
    to_addr     text NOT NULL CHECK (length(to_addr) BETWEEN 2 AND 320 AND to_addr ~ '@'),
    sender      boolean NOT NULL,
    recipient   boolean NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, from_addr),
    CHECK (sender OR recipient),
    CHECK (from_addr <> to_addr)
);

ALTER TABLE transport_rewrites ENABLE ROW LEVEL SECURITY;
ALTER TABLE transport_rewrites FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON transport_rewrites
    USING (tp_tenant_visible(tenant_id)) WITH CHECK (tp_tenant_visible(tenant_id));

SELECT tp_grant_roles();
