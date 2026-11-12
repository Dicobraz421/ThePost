-- Остановка синхронизации каталога при удалении больше порога учётных записей (SyncGate; CTR-35, решение
-- 2026-11-04-exchange-parity): одна запись на организацию, её подтверждает администратор (Admin API), снимает синхронизация.

CREATE TABLE sync_holds (
    tenant_id   uuid PRIMARY KEY REFERENCES tenants,
    removed     bigint NOT NULL CHECK (removed >= 0),
    total       bigint NOT NULL CHECK (total >= 0),
    detected_at timestamptz NOT NULL,
    approved    boolean NOT NULL
);

ALTER TABLE sync_holds ENABLE ROW LEVEL SECURITY;
ALTER TABLE sync_holds FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON sync_holds USING (tp_tenant_visible(tenant_id))
    WITH CHECK (tp_tenant_visible(tenant_id));

SELECT tp_grant_roles();
