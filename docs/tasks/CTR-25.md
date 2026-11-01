# CTR-25: контракты Admin API — политики уровней, домены, журнал аудита

lane: contracts
deps: [ADM-02, CTR-24, META-22]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md, crates/tp-meta/src/*, crates/tp-meta/migrations/*, crates/tp-testkit/src/*, crates/tp-admin/src/*]
read: [docs/CONTRACTS.md#Tenant, docs/CONTRACTS.md#Trash, Retention, Quota (Ф1), docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

## Сделать
- `PolicyStore` (`policy`, `set_policy`, `PolicyTarget`) в tp-contracts вместо временного трейта `tp-admin` (ADM-02) поверх уже готовых `PgMeta::policy`/`set_policy`.
- Домены организации (список, создание, удаление с проверкой учётных записей) — вместе с принятыми доменами CTR-24 (`AcceptedDomain`), без второго трейта; временный `DomainStore` `tp-admin` удалить.
- `AuditLog` (запись, чтение по организации и интервалу) поверх таблицы журнала аудита META-22 — реализация `AuditSink` `tp-admin`; `LogAudit` остаётся для экспорта в журнал узла.
- Реализации для `PgMeta` и in-memory в tp-testkit — в том же PR.

## Приёмка
- Общие наборы `tp_testkit::suite` для новых трейтов проходят на in-memory и на PostgreSQL.
