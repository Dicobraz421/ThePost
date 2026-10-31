-- ACL папок (RFC 4314) и делегирование отправки (META-11; SRS 4.5).

-- Запись ACL папки: идентификатор (имя входа, имя группы, `anyone`; с `-` в начале — отрицательные права
-- RFC 4314 §2) и права — строка `lrswipkxtea` в каноническом порядке. Пустые права не хранятся.
-- Связь только внутри учётной записи-владельца (папка удаляется — её ACL тоже).
CREATE TABLE mailbox_acl (
    tenant_id  uuid NOT NULL,
    account_id uuid NOT NULL,
    mailbox_id uuid NOT NULL,
    identifier text NOT NULL CHECK (length(identifier) BETWEEN 1 AND 255
                                    AND identifier !~ '[[:space:][:cntrl:]]' AND identifier <> '-'),
    rights     text NOT NULL CHECK (rights ~ '^l?r?s?w?i?p?k?x?t?e?a?$' AND rights <> ''),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, account_id, mailbox_id, identifier),
    FOREIGN KEY (tenant_id, account_id, mailbox_id) REFERENCES mailboxes ON DELETE CASCADE
);

-- Общие папки субъекта: по идентификатору.
CREATE INDEX mailbox_acl_identifier ON mailbox_acl (tenant_id, identifier);

-- Делегирование отправки от адресов учётной записи-владельца: представитель (имя входа или группы) и вид права.
CREATE TABLE send_delegations (
    tenant_id  uuid NOT NULL,
    account_id uuid NOT NULL,
    delegate   text NOT NULL CHECK (length(delegate) BETWEEN 1 AND 255
                                    AND delegate !~ '[[:space:][:cntrl:]]' AND left(delegate, 1) <> '-'),
    kind       text NOT NULL CHECK (kind IN ('send_as', 'send_on_behalf')),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, account_id, delegate),
    FOREIGN KEY (tenant_id, account_id) REFERENCES accounts ON DELETE CASCADE
);

CREATE INDEX send_delegations_delegate ON send_delegations (tenant_id, delegate);

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['mailbox_acl', 'send_delegations']
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I USING (tp_tenant_visible(tenant_id))'
                       ' WITH CHECK (tp_tenant_visible(tenant_id))', t);
    END LOOP;
END
$$;
