-- Журнал доставки и журнал аудита — только добавление (META-22; CONTRACTS.md#DeliveryLog; SRS 3.1, 5.2).
--
-- Изменять и удалять записи нельзя никому, в том числе владельцу схемы: триггер `tp_append_only` отклоняет
-- UPDATE и DELETE (код 42501). Роли приложения выдаются только SELECT и INSERT (META-23 по этому триггеру).
-- Журнал доставки секционирован по суткам (UTC); срок хранения — удаление секций целиком
-- (`tp_delivery_purge`). Секции создаёт и удаляет владелец схемы через функции SECURITY DEFINER: роль
-- приложения не выполняет DDL и не видит секции напрямую — только родительскую таблицу под RLS.

CREATE FUNCTION tp_append_only() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'таблица % только на добавление', TG_TABLE_NAME USING ERRCODE = 'insufficient_privilege';
END
$$;

-- ---------- Журнал доставки ----------

-- Порядок записи для равных моментов.
CREATE SEQUENCE delivery_log_seq;

-- sender_key, rcpt_keys — адреса в нижнем регистре ASCII для поиска; содержимое писем не хранится.
CREATE TABLE delivery_log (
    tenant_id  uuid NOT NULL,
    at         timestamptz NOT NULL,
    seq        bigint NOT NULL DEFAULT nextval('delivery_log_seq'),
    node       text NOT NULL,
    kind       text NOT NULL CHECK (kind IN ('receive', 'reject', 'deliver', 'relay', 'dsn')),
    sender     text NOT NULL,
    sender_key text NOT NULL,
    recipients text[] NOT NULL,
    rcpt_keys  text[] NOT NULL,
    message_id text,
    queue_id   uuid,
    decision   text NOT NULL
               CHECK (decision IN ('accept', 'reject', 'defer', 'quarantine', 'discard', 'hold')),
    reason     text
) PARTITION BY RANGE (at);

ALTER SEQUENCE delivery_log_seq OWNED BY delivery_log.seq;

CREATE INDEX delivery_log_time ON delivery_log (tenant_id, at, seq);
CREATE INDEX delivery_log_sender ON delivery_log (tenant_id, sender_key, at, seq);
CREATE INDEX delivery_log_message ON delivery_log (tenant_id, message_id) WHERE message_id IS NOT NULL;
CREATE INDEX delivery_log_rcpt ON delivery_log USING gin (rcpt_keys);

CREATE TRIGGER tp_append_only BEFORE UPDATE OR DELETE ON delivery_log
    FOR EACH ROW EXECUTE FUNCTION tp_append_only();

-- Секции суток `days` (UTC), которых ещё нет: `delivery_log_YYYYMMDD`. Параллельное создание той же секции
-- не ошибка.
CREATE FUNCTION tp_delivery_partitions(days date[]) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public, pg_temp AS $$
DECLARE
    d    date;
    part text;
BEGIN
    FOREACH d IN ARRAY days
    LOOP
        part := 'delivery_log_' || to_char(d, 'YYYYMMDD');
        IF to_regclass(format('public.%I', part)) IS NULL THEN
            BEGIN
                EXECUTE format('CREATE TABLE public.%I PARTITION OF public.delivery_log FOR VALUES FROM (%L) TO (%L)',
                               part, d::timestamp AT TIME ZONE 'UTC', (d + 1)::timestamp AT TIME ZONE 'UTC');
            EXCEPTION WHEN duplicate_table OR unique_violation OR invalid_object_definition THEN
                NULL;
            END;
        END IF;
    END LOOP;
END
$$;

-- Удаляет секции, сутки которых целиком раньше `before`; возвращает число удалённых.
CREATE FUNCTION tp_delivery_purge(before timestamptz) RETURNS integer
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public, pg_temp AS $$
DECLARE
    part text;
    n    integer := 0;
BEGIN
    FOR part IN
        SELECT c.relname FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid
        WHERE i.inhparent = 'public.delivery_log'::regclass AND c.relname ~ '^delivery_log_[0-9]{8}$'
        ORDER BY c.relname
    LOOP
        IF (to_date(substr(part, 14), 'YYYYMMDD') + 1)::timestamp AT TIME ZONE 'UTC' <= before THEN
            EXECUTE format('DROP TABLE public.%I', part);
            n := n + 1;
        END IF;
    END LOOP;
    RETURN n;
END
$$;

REVOKE ALL ON FUNCTION tp_delivery_partitions(date[]) FROM PUBLIC;
REVOKE ALL ON FUNCTION tp_delivery_purge(timestamptz) FROM PUBLIC;

-- ---------- Журнал аудита ----------

-- Операции администраторов (SRS 5.2, ADM-01): организация операции (`NULL` — уровень инсталляции, видна только
-- системным транзакциям), администратор, запись целиком (`record`). Тело запроса и секреты не записываются.
CREATE TABLE audit_log (
    seq          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    at           timestamptz NOT NULL,
    tenant_id    uuid,
    actor_tenant uuid NOT NULL,
    actor        uuid NOT NULL,
    record       jsonb NOT NULL
);

CREATE INDEX audit_log_tenant ON audit_log (tenant_id, at);

CREATE TRIGGER tp_append_only BEFORE UPDATE OR DELETE ON audit_log
    FOR EACH ROW EXECUTE FUNCTION tp_append_only();

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['delivery_log', 'audit_log']
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I USING (tp_tenant_visible(tenant_id))'
                       ' WITH CHECK (tp_tenant_visible(tenant_id))', t);
    END LOOP;
END
$$;
