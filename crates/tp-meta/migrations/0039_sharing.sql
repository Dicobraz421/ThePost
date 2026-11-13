-- Общий доступ к календарям и адресным книгам, представители, правила бронирования ресурсов, политика организации
-- (META-52; CTR-48, CONTRACTS.md#Sharing; SRS 4.4, 4.5; APP-I).

-- ---------- Права на коллекции (таблица ящика) ----------

-- Запись прав на коллекцию учётной записи-владельца: субъект (имя входа, имя группы, `anyone`; отрицательных прав
-- нет) и права, как выданы, без включённых младших. Пустые права не хранятся. Лежит на шарде ящика рядом с
-- коллекцией: удаление коллекции удаляет записи.
CREATE TABLE collection_acl (
    tenant_id     uuid NOT NULL,
    account_id    uuid NOT NULL,
    collection_id uuid NOT NULL,
    identifier    text NOT NULL CHECK (length(identifier) BETWEEN 1 AND 255
                                       AND identifier !~ '[[:space:][:cntrl:]]' AND left(identifier, 1) <> '-'),
    free_busy     boolean NOT NULL,
    read          boolean NOT NULL,
    write         boolean NOT NULL,
    schedule      boolean NOT NULL,
    admin         boolean NOT NULL,
    updated_at    timestamptz NOT NULL DEFAULT now(),
    CHECK (free_busy OR read OR write OR schedule OR admin),
    PRIMARY KEY (tenant_id, account_id, collection_id, identifier),
    FOREIGN KEY (tenant_id, account_id, collection_id) REFERENCES dav_collections ON DELETE CASCADE
);

-- Чужие коллекции субъекта (`shared_with`): по идентификатору.
CREATE INDEX collection_acl_identifier ON collection_acl (tenant_id, identifier);

-- ---------- Каталог (основная БД) ----------

-- Представители учётной записи: доступ ко всем её календарям и планирование от её имени. Удаление любой из
-- учётных записей удаляет запись.
CREATE TABLE account_delegates (
    tenant_id          uuid NOT NULL,
    account_id         uuid NOT NULL,
    delegate_id        uuid NOT NULL,
    level              text NOT NULL CHECK (level IN ('read', 'write')),
    schedule_on_behalf boolean NOT NULL,
    updated_at         timestamptz NOT NULL DEFAULT now(),
    CHECK (delegate_id <> account_id),
    PRIMARY KEY (tenant_id, account_id, delegate_id),
    FOREIGN KEY (tenant_id, account_id) REFERENCES accounts ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, delegate_id) REFERENCES accounts ON DELETE CASCADE
);

-- Чьим представителем является учётная запись (`delegations`).
CREATE INDEX account_delegates_delegate ON account_delegates (tenant_id, delegate_id);

-- Правила бронирования ресурса (BookingPolicy, JSON контракта) — только у учётной записи вида `resource`; смена вида
-- на пользователя снимает правила.
ALTER TABLE accounts
    ADD COLUMN booking_policy jsonb,
    ADD CONSTRAINT accounts_booking_policy_resource CHECK (booking_policy IS NULL OR kind = 'resource');

CREATE FUNCTION tp_booking_policy_kind() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.booking_policy := NULL;
    RETURN NEW;
END
$$;

CREATE TRIGGER tp_booking_policy_kind BEFORE UPDATE OF kind ON accounts
    FOR EACH ROW WHEN (NEW.kind <> 'resource' AND NEW.booking_policy IS NOT NULL)
    EXECUTE FUNCTION tp_booking_policy_kind();

-- Политика общего доступа организации (SharingPolicy, JSON контракта); нет строки — политика по умолчанию.
CREATE TABLE sharing_policies (
    tenant_id  uuid PRIMARY KEY REFERENCES tenants,
    policy     jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['collection_acl', 'account_delegates', 'sharing_policies']
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I USING (tp_tenant_visible(tenant_id))'
                       ' WITH CHECK (tp_tenant_visible(tenant_id))', t);
    END LOOP;
END
$$;

SELECT tp_grant_roles();
