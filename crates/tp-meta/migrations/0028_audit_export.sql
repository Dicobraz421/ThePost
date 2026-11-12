-- Выгрузка журнала аудита в SIEM (APP-59; решение владельца 12.11: база + выгрузка из неё).
--
-- Отметка доставки приёмника (`syslog`, `otlp`): номер последней доставленной записи `audit_log.seq`. Выгрузка
-- (роль maintenance под лизой) читает записи после отметки и сдвигает её после доставки; недоступный приёмник
-- догоняет пропущенное. Таблица уровня инсталляции, без организации: читает и пишет только системная транзакция.
CREATE TABLE audit_export (
    sink          text PRIMARY KEY CHECK (sink <> ''),
    delivered_seq bigint NOT NULL CHECK (delivered_seq >= 0),
    updated_at    timestamptz NOT NULL DEFAULT now()
);

SELECT tp_grant_roles();
