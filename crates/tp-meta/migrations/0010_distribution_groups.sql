-- Группы рассылки и очередь модерации (META-21; CONTRACTS.md#Groups; SRS 4.5).
-- Заменяют заготовки `groups` и `group_members` схемы v1: их не использовал ни один крейт, данных в них нет.
-- Списки группы (участники, ограничение отправителей, модераторы) хранятся с позицией `pos`: группа читается
-- такой, какой записана (порядок и повторы). Вложенные группы и группы в списке отправителей — без внешнего
-- ключа: неизвестная группа (каталог ещё не синхронизирован, группа удалена) при раскрытии пропускается.
-- Учётные записи — без внешнего ключа: активность учётной записи проверяет вызывающий.

DROP TABLE group_members;
DROP TABLE groups;

CREATE TABLE groups (
    tenant_id       uuid NOT NULL REFERENCES tenants,
    group_id        uuid NOT NULL,
    name            text NOT NULL CHECK (length(name) BETWEEN 1 AND 255 AND name !~ '[[:cntrl:]]'),
    address         text CHECK (octet_length(address) <= 320 AND position('@' IN address) > 0
                                AND address !~ '[[:space:][:cntrl:]]'),
    external_id     text CHECK (external_id <> '' AND octet_length(external_id) <= 255),
    senders         text NOT NULL DEFAULT 'anyone' CHECK (senders IN ('anyone', 'internal', 'only')),
    hidden_from_gal boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, group_id)
);

-- Адрес рассылки уникален в организации без учёта регистра; идентификатор каталога — уникален.
CREATE UNIQUE INDEX groups_address ON groups (tenant_id, lower(address)) WHERE address IS NOT NULL;
CREATE UNIQUE INDEX groups_external ON groups (tenant_id, external_id) WHERE external_id IS NOT NULL;

-- Участники: учётная запись или вложенная группа (ровно одно из двух).
CREATE TABLE group_members (
    tenant_id       uuid NOT NULL,
    group_id        uuid NOT NULL,
    pos             integer NOT NULL CHECK (pos >= 0),
    account_id      uuid,
    member_group_id uuid,
    CHECK ((account_id IS NULL) <> (member_group_id IS NULL)),
    PRIMARY KEY (tenant_id, group_id, pos),
    FOREIGN KEY (tenant_id, group_id) REFERENCES groups ON DELETE CASCADE
);

-- Ограничение `only`: разрешённые учётные записи и группы (участники — с раскрытием вложенных).
CREATE TABLE group_senders (
    tenant_id       uuid NOT NULL,
    group_id        uuid NOT NULL,
    pos             integer NOT NULL CHECK (pos >= 0),
    account_id      uuid,
    sender_group_id uuid,
    CHECK ((account_id IS NULL) <> (sender_group_id IS NULL)),
    PRIMARY KEY (tenant_id, group_id, pos),
    FOREIGN KEY (tenant_id, group_id) REFERENCES groups ON DELETE CASCADE
);

-- Модераторы: непусто — группа модерируемая.
CREATE TABLE group_moderators (
    tenant_id  uuid NOT NULL,
    group_id   uuid NOT NULL,
    pos        integer NOT NULL CHECK (pos >= 0),
    account_id uuid NOT NULL,
    PRIMARY KEY (tenant_id, group_id, pos),
    FOREIGN KEY (tenant_id, group_id) REFERENCES groups ON DELETE CASCADE
);

-- Очередь модерации. Ссылку на объект письма учитывает вызывающий; удаление группы забирает её письма
-- (`GroupStore::remove` возвращает их вызывающему), поэтому внешний ключ — без каскада.
CREATE TABLE group_held (
    tenant_id      uuid NOT NULL,
    held_id        uuid NOT NULL,
    group_id       uuid NOT NULL,
    sender         text NOT NULL,
    sender_account uuid,
    message_id     text,
    blob_hash      bytea NOT NULL CHECK (octet_length(blob_hash) = 32),
    blob_gen       bigint NOT NULL,
    size           bigint NOT NULL CHECK (size >= 0),
    held_at        timestamptz NOT NULL,
    PRIMARY KEY (tenant_id, held_id),
    FOREIGN KEY (tenant_id, group_id) REFERENCES groups
);

CREATE INDEX group_held_order ON group_held (tenant_id, held_at, held_id);
CREATE INDEX group_held_group ON group_held (tenant_id, group_id, held_at, held_id);

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['groups', 'group_members', 'group_senders', 'group_moderators', 'group_held']
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I USING (tp_tenant_visible(tenant_id))'
                       ' WITH CHECK (tp_tenant_visible(tenant_id))', t);
    END LOOP;
END
$$;
