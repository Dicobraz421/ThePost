-- Журнал отправленных автоответов (META-50; RFC 5230 `:days`, `:handle`; CONTRACTS.md#Sieve).
--
-- Таблица ящика на шарде учётной записи. Ключ — SHA-256 от «отправитель NUL handle»: адрес отправителя открытым
-- текстом не хранится, длина ключа не зависит от длины `:handle`. Записи старше meta.sieve.vacation_log_ttl
-- удаляет роль maintenance.
CREATE TABLE vacation_log (
    tenant_id  uuid NOT NULL,
    account_id uuid NOT NULL,
    digest     bytea NOT NULL CHECK (octet_length(digest) = 32),
    -- Время последнего отправленного автоответа этому отправителю по этому `:handle`.
    replied_at timestamptz NOT NULL,
    PRIMARY KEY (tenant_id, account_id, digest)
);

CREATE INDEX vacation_log_replied ON vacation_log (replied_at);

ALTER TABLE vacation_log ENABLE ROW LEVEL SECURITY;
ALTER TABLE vacation_log FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON vacation_log USING (tp_tenant_visible(tenant_id))
    WITH CHECK (tp_tenant_visible(tenant_id));

SELECT tp_grant_roles();
