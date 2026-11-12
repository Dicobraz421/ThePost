-- Отправки JMAP и аудит доступа к ящику (x/jmap-08; CONTRACTS.md#Identity и EmailSubmission, #MailboxAudit;
-- CTR-30, CTR-35, CTR-42; SRS 5.2).
--
-- Отправки — таблицы ящика на шарде учётной записи (META-30): записи `email_submissions`, надгробия удалённых
-- `email_submission_tombstones` (для `EmailSubmission/changes`), состояние и нижняя граница истории — в
-- `mailbox_state`. Аудит ящика — в основной БД рядом с журналом доставки: таблица только на добавление,
-- секционирована по суткам UTC; секции создаёт и удаляет владелец схемы через функции SECURITY DEFINER
-- `tp_log_partitions` и `tp_log_purge`, общие с журналом доставки (заменяют функции журнала доставки из 0011).

-- ---------- Отправки ----------

ALTER TABLE mailbox_state
    -- Состояние отправок (JMAP state EmailSubmission): растёт при каждом изменении.
    ADD COLUMN submission_state bigint NOT NULL DEFAULT 0,
    -- Надгробия с состоянием <= submission_floor удалены: изменения от более раннего состояния — Conflict.
    ADD COLUMN submission_floor bigint NOT NULL DEFAULT 0;

CREATE TABLE email_submissions (
    tenant_id     uuid NOT NULL,
    account_id    uuid NOT NULL,
    id            text NOT NULL CHECK (octet_length(id) BETWEEN 1 AND 64),
    identity      text NOT NULL CHECK (octet_length(identity) BETWEEN 1 AND 64),
    email_id      uuid NOT NULL,
    mail_from     text NOT NULL,
    rcpt_to       text[] NOT NULL,
    send_at       timestamptz NOT NULL,
    undo_status   text NOT NULL CHECK (undo_status IN ('pending', 'final', 'canceled')),
    -- Статусы получателей: массив пар [адрес, SubmitStatus] в порядке записи.
    delivery      jsonb NOT NULL DEFAULT '[]',
    queue_id      uuid,
    -- Состояние при создании и при последнем изменении.
    created_state bigint NOT NULL,
    changed_state bigint NOT NULL,
    PRIMARY KEY (tenant_id, account_id, id)
);

CREATE INDEX email_submissions_order ON email_submissions (tenant_id, account_id, send_at, id);
CREATE INDEX email_submissions_changed ON email_submissions (tenant_id, account_id, changed_state);
CREATE INDEX email_submissions_due ON email_submissions (send_at, id) WHERE undo_status = 'pending';
CREATE INDEX email_submissions_done ON email_submissions (send_at, id) WHERE undo_status <> 'pending';

CREATE TABLE email_submission_tombstones (
    tenant_id       uuid NOT NULL,
    account_id      uuid NOT NULL,
    id              text NOT NULL,
    created_state   bigint NOT NULL,
    destroyed_state bigint NOT NULL,
    destroyed_at    timestamptz NOT NULL,
    PRIMARY KEY (tenant_id, account_id, id)
);

CREATE INDEX email_submission_tombstones_state
    ON email_submission_tombstones (tenant_id, account_id, destroyed_state);
CREATE INDEX email_submission_tombstones_at ON email_submission_tombstones (destroyed_at);

-- ---------- Журналы по суткам: общие функции секций ----------

-- Секции суток `days` (UTC) журнала `parent`, которых ещё нет: `<parent>_YYYYMMDD`. Параллельное создание той же
-- секции не ошибка.
CREATE FUNCTION tp_log_partitions(parent text, days date[]) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public, pg_temp AS $$
DECLARE
    d    date;
    part text;
BEGIN
    IF parent NOT IN ('delivery_log', 'mailbox_audit') THEN
        RAISE EXCEPTION 'журнал % не секционирован по суткам', parent USING ERRCODE = 'invalid_parameter_value';
    END IF;
    FOREACH d IN ARRAY days
    LOOP
        part := parent || '_' || to_char(d, 'YYYYMMDD');
        IF to_regclass(format('public.%I', part)) IS NULL THEN
            BEGIN
                EXECUTE format('CREATE TABLE public.%I PARTITION OF public.%I FOR VALUES FROM (%L) TO (%L)',
                               part, parent, d::timestamp AT TIME ZONE 'UTC', (d + 1)::timestamp AT TIME ZONE 'UTC');
            EXCEPTION WHEN duplicate_table OR unique_violation OR invalid_object_definition THEN
                NULL;
            END;
        END IF;
    END LOOP;
END
$$;

-- Удаляет секции журнала `parent`, сутки которых целиком раньше `before`; возвращает число удалённых.
CREATE FUNCTION tp_log_purge(parent text, before timestamptz) RETURNS integer
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public, pg_temp AS $$
DECLARE
    part text;
    n    integer := 0;
BEGIN
    IF parent NOT IN ('delivery_log', 'mailbox_audit') THEN
        RAISE EXCEPTION 'журнал % не секционирован по суткам', parent USING ERRCODE = 'invalid_parameter_value';
    END IF;
    FOR part IN
        SELECT c.relname FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid
        WHERE i.inhparent = format('public.%I', parent)::regclass AND c.relname ~ ('^' || parent || '_[0-9]{8}$')
        ORDER BY c.relname
    LOOP
        IF (to_date(right(part, 8), 'YYYYMMDD') + 1)::timestamp AT TIME ZONE 'UTC' <= before THEN
            EXECUTE format('DROP TABLE public.%I', part);
            n := n + 1;
        END IF;
    END LOOP;
    RETURN n;
END
$$;

REVOKE ALL ON FUNCTION tp_log_partitions(text, date[]) FROM PUBLIC;
REVOKE ALL ON FUNCTION tp_log_purge(text, timestamptz) FROM PUBLIC;

-- Статус доставки отправки: события записи очереди (`DeliveryLog::by_queue`).
CREATE INDEX delivery_log_queue ON delivery_log (tenant_id, queue_id, at, seq) WHERE queue_id IS NOT NULL;

-- ---------- Аудит ящика ----------

-- Порядок записи для равных моментов.
CREATE SEQUENCE mailbox_audit_seq;

-- actor_key — исполнитель в нижнем регистре ASCII для поиска; содержимое писем не хранится.
CREATE TABLE mailbox_audit (
    tenant_id     uuid NOT NULL,
    account_id    uuid NOT NULL,
    at            timestamptz NOT NULL,
    seq           bigint NOT NULL DEFAULT nextval('mailbox_audit_seq'),
    actor         text NOT NULL,
    actor_key     text NOT NULL,
    actor_account uuid,
    actor_kind    text NOT NULL CHECK (actor_kind IN ('owner', 'delegate', 'admin')),
    action        text NOT NULL CHECK (action IN ('login', 'read', 'move', 'delete', 'send', 'send_as',
                                                  'send_on_behalf', 'change_rights', 'change_rules',
                                                  'change_forwarding')),
    mailbox_id    uuid,
    message_id    uuid,
    detail        text,
    client        text
) PARTITION BY RANGE (at);

ALTER SEQUENCE mailbox_audit_seq OWNED BY mailbox_audit.seq;

CREATE INDEX mailbox_audit_time ON mailbox_audit (tenant_id, at, seq);
CREATE INDEX mailbox_audit_account ON mailbox_audit (tenant_id, account_id, at, seq);
CREATE INDEX mailbox_audit_actor ON mailbox_audit (tenant_id, actor_key, at, seq);

CREATE TRIGGER tp_append_only BEFORE UPDATE OR DELETE ON mailbox_audit
    FOR EACH ROW EXECUTE FUNCTION tp_append_only();

-- Политика аудита организации (account_id IS NULL) или ящика.
CREATE TABLE mailbox_audit_policies (
    tenant_id     uuid NOT NULL REFERENCES tenants ON DELETE CASCADE,
    account_id    uuid,
    non_owner     boolean NOT NULL,
    owner_actions text[] NOT NULL,
    UNIQUE NULLS NOT DISTINCT (tenant_id, account_id)
);

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['email_submissions', 'email_submission_tombstones', 'mailbox_audit',
                             'mailbox_audit_policies']
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I USING (tp_tenant_visible(tenant_id))'
                       ' WITH CHECK (tp_tenant_visible(tenant_id))', t);
    END LOOP;
END
$$;

-- Права ролей: общие функции секций вместо функций журнала доставки.
CREATE OR REPLACE FUNCTION tp_grant_roles() RETURNS void
LANGUAGE plpgsql SET search_path = public, pg_temp AS $$
DECLARE
    t record;
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'thepost_app') THEN
        GRANT USAGE ON SCHEMA public TO thepost_app;
        GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO thepost_app;
        GRANT EXECUTE ON FUNCTION tp_log_partitions(text, date[]), tp_log_purge(text, timestamptz) TO thepost_app;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'thepost_reader') THEN
        GRANT USAGE ON SCHEMA public TO thepost_reader;
    END IF;
    FOR t IN
        SELECT c.relname,
               EXISTS (SELECT 1 FROM pg_trigger g WHERE g.tgrelid = c.oid AND g.tgname = 'tp_append_only')
                   AS append_only
        FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p') AND NOT c.relispartition
          AND c.relname <> '_sqlx_migrations'
    LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'thepost_app') THEN
            EXECUTE format('REVOKE ALL ON public.%I FROM thepost_app', t.relname);
            IF t.append_only THEN
                EXECUTE format('GRANT SELECT, INSERT ON public.%I TO thepost_app', t.relname);
            ELSE
                EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON public.%I TO thepost_app', t.relname);
            END IF;
        END IF;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'thepost_reader') THEN
            EXECUTE format('REVOKE ALL ON public.%I FROM thepost_reader', t.relname);
            IF t.relname IN ('nodes', 'leases', 'event_cursors', 'index_manifests') THEN
                EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON public.%I TO thepost_reader', t.relname);
            ELSE
                EXECUTE format('GRANT SELECT ON public.%I TO thepost_reader', t.relname);
            END IF;
        END IF;
    END LOOP;
END
$$;

DROP FUNCTION tp_delivery_partitions(date[]);
DROP FUNCTION tp_delivery_purge(timestamptz);

SELECT tp_grant_roles();
