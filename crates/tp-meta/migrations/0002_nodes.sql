-- Членство узлов (CTR-14, META-16).

-- Запись узла продлевает сам узел (Membership::heartbeat); живые — с expires_at > now() по часам СУБД.
-- Таблица без RLS: состав кластера общий для всех организаций.
CREATE TABLE nodes (
    node_id    text PRIMARY KEY CHECK (length(node_id) BETWEEN 1 AND 255 AND node_id !~ '[[:space:][:cntrl:]]'),
    roles      text[] NOT NULL DEFAULT '{}',
    joined_at  timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL
);

CREATE INDEX nodes_expires_at ON nodes (expires_at);
