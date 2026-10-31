-- Очередь исходящей почты (CTR-06, META-09; SRS 4.1).
-- Запись — письмо (один объект BLOB) и его получатели. Захват записи узлом — лиза `lease_until` с номером
-- захвата `fencing`; время сравнивается с `now` вызывающего (тестовые часы), не с часами СУБД.
-- `next_attempt` записи — ближайший срок попытки незавершённых получателей (поддерживает реализация):
-- по нему claim выбирает записи по индексу. Запись, все получатели которой завершены, удаляется.

CREATE TABLE queue (
    tenant_id    uuid NOT NULL REFERENCES tenants,
    queue_id     uuid PRIMARY KEY,
    blob_hash    bytea NOT NULL CHECK (octet_length(blob_hash) = 32),
    blob_gen     bigint NOT NULL,
    size         bigint NOT NULL CHECK (size >= 0),
    mail_from    text NOT NULL,
    envid        text,
    ret          text CHECK (ret IN ('full', 'hdrs')),
    queued_at    timestamptz NOT NULL,
    next_attempt timestamptz NOT NULL,
    fencing      bigint NOT NULL DEFAULT 0,
    lease_until  timestamptz
);

CREATE INDEX queue_next_attempt ON queue (next_attempt);

-- notify: NULL — не задан; иначе битовая маска 1 = SUCCESS, 2 = FAILURE, 4 = DELAY (0 — NEVER).
CREATE TABLE queue_rcpts (
    tenant_id    uuid NOT NULL,
    queue_id     uuid NOT NULL REFERENCES queue ON DELETE CASCADE,
    ord          integer NOT NULL,
    address      text NOT NULL,
    notify       smallint CHECK (notify BETWEEN 0 AND 7),
    orcpt        text,
    status       text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'delivered', 'failed')),
    next_attempt timestamptz NOT NULL,
    attempts     integer NOT NULL DEFAULT 0,
    last_error   text,
    reply        text,
    PRIMARY KEY (queue_id, address)
);

CREATE INDEX queue_rcpts_pending ON queue_rcpts (queue_id, next_attempt) WHERE status = 'pending';

-- Данные писем — организации: RLS как у таблиц ящика; claim и операции захвата — системные транзакции.
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['queue', 'queue_rcpts']
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I USING (tp_tenant_visible(tenant_id))'
                       ' WITH CHECK (tp_tenant_visible(tenant_id))', t);
    END LOOP;
END
$$;
