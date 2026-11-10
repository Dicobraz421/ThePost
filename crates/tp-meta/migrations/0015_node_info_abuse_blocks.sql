-- Записи узлов с адресом и площадкой, блокировки источников и учётных записей (META-27; CTR-23,
-- решения 2026-11-01 «Взаимодействие компонентов», «Масштабирование, шарды, геораспределение»; SRS 2.8, 6.3).

-- Адрес служебного канала узла (`https://host:port`, пусто — запросов не принимает) и площадка (`SiteId`).
-- Прежние записи — без адреса на площадке по умолчанию; узел заменит их при следующем продлении.
ALTER TABLE nodes
    ADD COLUMN addr text NOT NULL DEFAULT '' CHECK (length(addr) <= 2048 AND addr !~ '[[:space:][:cntrl:]]'),
    ADD COLUMN site text NOT NULL DEFAULT 'default'
        CHECK (length(site) BETWEEN 1 AND 255 AND site !~ '[[:space:][:cntrl:]]');

-- Блокировки организации (AbuseBlocks). Субъект — ключ `subject`: `ip:<адрес>`, `net:<адрес>/<префикс>`,
-- `account:<uuid>`; адрес и подсеть — также `net` (cidr: адрес — /32 или /128) для поиска покрывающих подсетей.
-- Истёкшие строки не действуют; новая блокировка того же субъекта заменяет истёкшую. Срок — часы СУБД.
-- Без внешнего ключа на tenants: блокировки — служебные записи с ограниченным сроком.
CREATE TABLE abuse_blocks (
    tenant_id  uuid NOT NULL,
    subject    text NOT NULL CHECK (length(subject) BETWEEN 1 AND 128),
    kind       text NOT NULL CHECK (kind IN ('ip', 'net', 'account')),
    net        cidr,
    account_id uuid,
    until      timestamptz NOT NULL,
    reason     text NOT NULL CHECK (length(reason) <= 1024),
    blocked_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, subject),
    CHECK ((kind = 'account') = (account_id IS NOT NULL) AND (kind = 'account') = (net IS NULL))
);

CREATE INDEX abuse_blocks_net ON abuse_blocks USING gist (net inet_ops) WHERE net IS NOT NULL;
CREATE INDEX abuse_blocks_until ON abuse_blocks (tenant_id, until);

ALTER TABLE abuse_blocks ENABLE ROW LEVEL SECURITY;
ALTER TABLE abuse_blocks FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON abuse_blocks USING (tp_tenant_visible(tenant_id))
    WITH CHECK (tp_tenant_visible(tenant_id));

SELECT tp_grant_roles();
