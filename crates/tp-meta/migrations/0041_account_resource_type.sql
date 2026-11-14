-- Тип ресурса бронирования (CTR-88, решение 121; CONTRACTS.md#Metadata): помещение (`room`) или оборудование
-- (`equipment`). Задаётся записью учётной записи (AccountUpsert::resource_type): у ресурса — всегда, у пользователя —
-- NULL; смена вида на пользователя снимает тип. Прежние ресурсы — помещения.

ALTER TABLE accounts
    ADD COLUMN resource_type text CHECK (resource_type IN ('room', 'equipment'));

-- Транзакция миграции видит строки всех организаций.
SELECT set_config('app.all_tenants', 'on', true);
UPDATE accounts SET resource_type = 'room' WHERE kind = 'resource';
SELECT set_config('app.all_tenants', '', true);

ALTER TABLE accounts
    ADD CONSTRAINT accounts_resource_type_kind CHECK ((kind = 'resource') = (resource_type IS NOT NULL));
