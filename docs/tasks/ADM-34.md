# ADM-34: Admin API на контрактах CTR-25: политики и журнал аудита из БД

lane: admin
deps: [CTR-25, META-70]
class: B
files: [crates/tp-admin/src/*, crates/thepost/src/services.rs]
read: [docs/tasks/CTR-25.md, docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 6 и 12; решение 98, 104.

Класс B: удаляется тип, который подставляет узел, — место вызова в `crates/thepost/src/services.rs` правится в том же PR
(межполосная по устройству, как контракт с вызовами — решение 71); ветка `x/…`.

## Сделать
- Политики — `tp_contracts::PolicyStore` (CTR-25): временный трейт `tp_admin::PolicyStore`, своя копия `PolicyTarget`
  и преобразование в `services.rs` удалить (один `PolicyTarget` — в контракте); `PgPolicies` удалить.
- Журнал аудита — `AuditLog` (CTR-25, таблица `audit_log`, META-22): запись каждого изменения; `GET …/audit` — данные
  из таблицы вместо 503; `LogAudit` как приёмник по умолчанию удалить (экспорт в syslog — `admin.audit.syslog`, как был).

## Приёмка
- Тесты: изменение через Admin API видно в `GET …/audit`; политика уровня читается и пишется через контракт;
  интеграционный тест узла на PostgreSQL.
