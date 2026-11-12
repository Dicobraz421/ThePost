-- Занятость повторяющихся событий разворачивается при запросе (META-54; решение 59; CONTRACTS.md#Schedule, CTR-47):
-- индекс занятости dav_busy — только у неповторяющихся объектов, горизонта индекса нет.
--
-- Таблицы ящика: миграция идёт на каждом шарде.

ALTER TABLE dav_objects
    -- Повторяющееся событие (в теле RRULE или RDATE): строк dav_busy нет, занятость разворачивается из тела при запросе.
    ADD COLUMN recurring boolean NOT NULL DEFAULT false
        CHECK (NOT recurring OR component = 'VEVENT');

-- Уже записанные события с правилами повторения (META-51 писал их периоды в dav_busy до горизонта). Свойство — в
-- начале строки содержимого (RFC 5545 §3.1); название свойства клиенты не переносят.
UPDATE dav_objects
SET recurring = true
WHERE component = 'VEVENT' AND body ~* '(^|\n)(RRULE|RDATE)[;:]';

-- Их периоды из индекса убираются: иначе занятость до горизонта считалась бы дважды, а после него — терялась.
DELETE FROM dav_busy b
USING dav_objects o
WHERE o.tenant_id = b.tenant_id AND o.account_id = b.account_id AND o.object_id = b.object_id AND o.recurring;

-- Запрос занятости: повторяющиеся события учётных записей (их мало — частичный индекс).
CREATE INDEX dav_objects_recurring ON dav_objects (tenant_id, account_id) WHERE recurring;
