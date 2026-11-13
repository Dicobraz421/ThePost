-- Событие «приём iTIP» в журнале доставки (CTR-66; CONTRACTS.md#DeliveryLog): итог применения приглашения из письма
-- iMIP — решение accept или discard, метод и причина — в reason; содержимое не хранится.

ALTER TABLE delivery_log DROP CONSTRAINT delivery_log_kind_check;

ALTER TABLE delivery_log
    ADD CONSTRAINT delivery_log_kind_check CHECK (kind IN ('receive', 'reject', 'deliver', 'relay', 'dsn', 'itip'));
