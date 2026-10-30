-- Схема метаданных v1 (SRS 3.1, 3.4, 3.6, 3.8).
-- Ключи таблиц ящика содержат tenant_id и account_id; внешних ключей между ящиками нет (шардирование, SRS 3.6).
-- RLS по tenant_id: транзакция начинается с set_config('app.tenant_id', $1, true) (= SET LOCAL, совместимо
-- с PgBouncer transaction); системные операции (GC BLOB, журнал событий) — set_config('app.all_tenants', 'on', true).
-- Владелец таблиц тоже подчиняется RLS (FORCE); суперпользователь и BYPASSRLS — нет (проверяет doctor).

CREATE FUNCTION tp_tenant_visible(t uuid) RETURNS boolean
LANGUAGE sql STABLE AS $$
    SELECT t = NULLIF(current_setting('app.tenant_id', true), '')::uuid
        OR coalesce(current_setting('app.all_tenants', true), '') = 'on'
$$;

-- ---------- Каталог ----------

CREATE TABLE tenants (
    tenant_id  uuid PRIMARY KEY,
    name       text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- Организация по умолчанию (TenantId::DEFAULT).
INSERT INTO tenants (tenant_id, name) VALUES ('00000000-0000-0000-0000-000000000001', 'default');

-- Справочник шардов: в базовой поставке один шард — эта БД (dsn IS NULL).
CREATE TABLE shards (
    shard_id    integer PRIMARY KEY,
    name        text NOT NULL UNIQUE,
    dsn         text,
    accepts_new boolean NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT now()
);

INSERT INTO shards (shard_id, name) VALUES (0, 'local');

CREATE TABLE domains (
    tenant_id  uuid NOT NULL REFERENCES tenants,
    domain_id  uuid NOT NULL,
    name       text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, domain_id)
);

-- Имя домена уникально в инсталляции (маршрутизация входящей почты).
CREATE UNIQUE INDEX domains_name ON domains (lower(name));

CREATE TABLE accounts (
    tenant_id         uuid NOT NULL,
    account_id        uuid NOT NULL,
    domain_id         uuid NOT NULL,
    login             text NOT NULL,
    display_name      text NOT NULL DEFAULT '',
    external_id       text,
    enabled           boolean NOT NULL DEFAULT true,
    shard_id          integer NOT NULL DEFAULT 0 REFERENCES shards,
    -- Счётчик ModSeq учётной записи (CONTRACTS.md#Metadata).
    modseq            bigint NOT NULL DEFAULT 0,
    -- Изменения с modseq <= changelog_floor удалены из журнала: since ниже — Conflict.
    changelog_floor   bigint NOT NULL DEFAULT 0,
    -- Последний выданный UIDVALIDITY папок (строго растёт).
    uid_validity_last bigint NOT NULL DEFAULT 0,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, account_id),
    FOREIGN KEY (tenant_id, domain_id) REFERENCES domains
);

CREATE UNIQUE INDEX accounts_login ON accounts (tenant_id, lower(login));
CREATE UNIQUE INDEX accounts_external ON accounts (tenant_id, external_id) WHERE external_id IS NOT NULL;

CREATE TABLE groups (
    tenant_id   uuid NOT NULL REFERENCES tenants,
    group_id    uuid NOT NULL,
    name        text NOT NULL,
    email       text,
    external_id text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, group_id)
);

CREATE UNIQUE INDEX groups_name ON groups (tenant_id, lower(name));

CREATE TABLE group_members (
    tenant_id  uuid NOT NULL,
    group_id   uuid NOT NULL,
    account_id uuid NOT NULL,
    PRIMARY KEY (tenant_id, group_id, account_id),
    FOREIGN KEY (tenant_id, group_id) REFERENCES groups ON DELETE CASCADE
);

CREATE INDEX group_members_account ON group_members (tenant_id, account_id);

-- ---------- Ящик: папки и письма ----------

CREATE TABLE mailboxes (
    tenant_id      uuid NOT NULL,
    account_id     uuid NOT NULL,
    mailbox_id     uuid NOT NULL,
    parent_id      uuid,
    name           text NOT NULL,
    role           text CHECK (role IN ('inbox', 'archive', 'drafts', 'junk', 'sent', 'trash')),
    uid_validity   bigint NOT NULL CHECK (uid_validity BETWEEN 1 AND 4294967295),
    uid_next       bigint NOT NULL DEFAULT 1,
    highest_modseq bigint NOT NULL DEFAULT 0,
    subscribed     boolean NOT NULL DEFAULT true,
    created_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, account_id, mailbox_id)
);

CREATE UNIQUE INDEX mailboxes_name ON mailboxes (tenant_id, account_id, parent_id, name) NULLS NOT DISTINCT;
CREATE UNIQUE INDEX mailboxes_role ON mailboxes (tenant_id, account_id, role) WHERE role IS NOT NULL;

CREATE TABLE messages (
    tenant_id     uuid NOT NULL,
    account_id    uuid NOT NULL,
    message_id    uuid NOT NULL,
    mailbox_id    uuid NOT NULL,
    uid           bigint NOT NULL,
    modseq        bigint NOT NULL,
    size          bigint NOT NULL,
    received_at   timestamptz NOT NULL,
    flags         text[] NOT NULL DEFAULT '{}',
    skeleton_hash bytea NOT NULL,
    skeleton_gen  bigint NOT NULL,
    thread_key    text NOT NULL DEFAULT '',
    meta          jsonb NOT NULL,
    -- Письмо в корзине: исходная папка и момент переноса (SRS 4.7).
    trashed_from  uuid,
    trashed_at    timestamptz,
    PRIMARY KEY (tenant_id, account_id, message_id)
);

CREATE UNIQUE INDEX messages_uid ON messages (tenant_id, account_id, mailbox_id, uid);
CREATE INDEX messages_modseq ON messages (tenant_id, account_id, mailbox_id, modseq);
CREATE INDEX messages_thread ON messages (tenant_id, account_id, thread_key);

-- Объекты частей письма (модель «скелет + части»); объект принадлежит организации письма.
CREATE TABLE message_parts (
    tenant_id  uuid NOT NULL,
    account_id uuid NOT NULL,
    message_id uuid NOT NULL,
    part       text NOT NULL,
    blob_hash  bytea NOT NULL,
    blob_gen   bigint NOT NULL,
    size       bigint NOT NULL,
    PRIMARY KEY (tenant_id, account_id, message_id, part),
    FOREIGN KEY (tenant_id, account_id, message_id) REFERENCES messages ON DELETE CASCADE
);

-- Журнал изменений (IMAP QRESYNC, JMAP /changes). object: 'message' | 'mailbox';
-- op 'moved' — письмо сменило папку с тем же message_id (корзина): из from_mailbox_id/from_uid в mailbox_id/uid.
CREATE TABLE changelog (
    tenant_id       uuid NOT NULL,
    account_id      uuid NOT NULL,
    modseq          bigint NOT NULL,
    object          text NOT NULL CHECK (object IN ('message', 'mailbox')),
    object_id       uuid NOT NULL,
    op              text NOT NULL CHECK (op IN ('created', 'updated', 'destroyed', 'moved')),
    mailbox_id      uuid,
    uid             bigint,
    from_mailbox_id uuid,
    from_uid        bigint,
    PRIMARY KEY (tenant_id, account_id, modseq, object, object_id, op)
);

-- ---------- BLOB (SRS 3.4) ----------

-- Одна строка на хэш в организации. state 'deleted' — надгробие после finalize_delete: хранит поколение,
-- чтобы загрузка, начатая до удаления, получила Bumped; удаляется по истечении срока (BlobIndexConfig).
CREATE TABLE blobs (
    tenant_id  uuid NOT NULL,
    hash       bytea NOT NULL CHECK (octet_length(hash) = 32),
    gen        bigint NOT NULL,
    refcnt     bigint NOT NULL CHECK (refcnt >= 0),
    state      text NOT NULL DEFAULT 'live' CHECK (state IN ('live', 'deleting', 'deleted')),
    size       bigint NOT NULL,
    zero_since timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, hash)
);

CREATE INDEX blobs_gc ON blobs (zero_since) WHERE refcnt = 0;

-- ---------- Журнал событий ядра (SRS 2.5) ----------

-- Счётчик позиций раздела: строка блокируется до фиксации транзакции, поэтому позиции видны по порядку.
CREATE TABLE event_partitions (
    partition integer PRIMARY KEY,
    last_pos  bigint NOT NULL
);

CREATE TABLE events (
    partition  integer NOT NULL,
    pos        bigint NOT NULL,
    tenant_id  uuid NOT NULL,
    account_id uuid NOT NULL,
    mailbox_id uuid,
    kind       jsonb NOT NULL,
    at         timestamptz NOT NULL,
    PRIMARY KEY (partition, pos)
);

CREATE TABLE event_cursors (
    subscriber text NOT NULL,
    partition  integer NOT NULL,
    pos        bigint NOT NULL DEFAULT 0,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (subscriber, partition)
);

-- ---------- Лизы ----------

CREATE TABLE leases (
    name       text PRIMARY KEY,
    holder     text NOT NULL,
    fencing    bigint NOT NULL,
    expires_at timestamptz NOT NULL
);

-- ---------- RLS ----------

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['tenants', 'domains', 'accounts', 'groups', 'group_members', 'mailboxes',
                             'messages', 'message_parts', 'changelog', 'blobs', 'events']
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I USING (tp_tenant_visible(tenant_id))'
                       ' WITH CHECK (tp_tenant_visible(tenant_id))', t);
    END LOOP;
END
$$;
