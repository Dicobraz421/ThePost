-- Корзина и политики (CTR-07, META-10; SRS 4.7, 3.8).

-- Элемент корзины — письмо с trashed_at (перенос в корзину: исходная папка trashed_from, момент trashed_at).
-- Срок окончательного удаления фиксируется при переносе: изменение политики действует на элементы,
-- удалённые после него; TrashStore::defer переносит срок (Legal Hold).
ALTER TABLE messages ADD COLUMN trash_expires_at timestamptz;

-- Письма, перенесённые в корзину до этой миграции: срок по умолчанию (30 суток).
UPDATE messages SET trash_expires_at = trashed_at + interval '30 days' WHERE trashed_at IS NOT NULL;

CREATE INDEX messages_trash_expires ON messages (trash_expires_at) WHERE trash_expires_at IS NOT NULL;
CREATE INDEX messages_trashed ON messages (tenant_id, account_id, trashed_at) WHERE trashed_at IS NOT NULL;

-- Политики уровня цепочки организация → домен → ящик (учётная запись) → папка (PolicyResolver).
-- object_id: организация — tenant_id, иначе идентификатор домена, учётной записи или папки.
-- NULL — не задано на этом уровне. Длительности — микросекунды.
CREATE TABLE policies (
    tenant_id          uuid NOT NULL REFERENCES tenants,
    level              text NOT NULL CHECK (level IN ('tenant', 'domain', 'account', 'mailbox')),
    object_id          uuid NOT NULL,
    quota_bytes        bigint CHECK (quota_bytes >= 0),
    trash_retention_us bigint CHECK (trash_retention_us >= 0),
    retention_us       bigint CHECK (retention_us >= 0),
    send_per_hour      bigint CHECK (send_per_hour BETWEEN 0 AND 4294967295),
    max_recipients     bigint CHECK (max_recipients BETWEEN 0 AND 4294967295),
    dkim_selector      text,
    milter_profile     text,
    updated_at         timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, level, object_id),
    CHECK (level <> 'tenant' OR object_id = tenant_id)
);

ALTER TABLE policies ENABLE ROW LEVEL SECURITY;
ALTER TABLE policies FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON policies USING (tp_tenant_visible(tenant_id))
    WITH CHECK (tp_tenant_visible(tenant_id));

-- Срок корзины для письма учётной записи a из папки m: самый специфичный заданный уровень; NULL — не задан
-- (значение по умолчанию конфигурации). Вызывается при переносе в корзину; подчиняется RLS вызывающего.
CREATE FUNCTION tp_trash_retention_us(t uuid, a uuid, m uuid) RETURNS bigint
LANGUAGE sql STABLE AS $$
    SELECT p.trash_retention_us FROM policies p
    WHERE p.tenant_id = t AND p.trash_retention_us IS NOT NULL
      AND ((p.level = 'tenant' AND p.object_id = t)
        OR (p.level = 'domain'
            AND p.object_id = (SELECT domain_id FROM accounts WHERE tenant_id = t AND account_id = a))
        OR (p.level = 'account' AND p.object_id = a)
        OR (p.level = 'mailbox' AND p.object_id = m))
    ORDER BY CASE p.level WHEN 'mailbox' THEN 0 WHEN 'account' THEN 1 WHEN 'domain' THEN 2 ELSE 3 END
    LIMIT 1
$$;
