# CTR-25: контракты Admin API — политики уровней и журнал аудита

lane: contracts
deps: [ADM-02, CTR-24, META-22]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md#Tenant, docs/spec/S5.md#5.2, docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Аудит 12.11 (решения 93, 94, 98, 104): домены организации — один API транспорта (`/transport/domains`, ADM-33), отдельного
контракта доменов нет; реализации — по полосам: PostgreSQL — META-70, переход Admin API и узла — ADM-34.

## Сделать
- `PolicyStore` (`policy`, `set_policy`, `PolicyTarget`) в tp-contracts вместо временного трейта `tp-admin` (ADM-02);
  один `PolicyTarget` на всю систему.
- `AuditLog` (запись, чтение по организации, интервалу и курсору) поверх таблицы журнала аудита META-22.
- Реализации in-memory в tp-testkit и общие наборы `suite`; `docs/CONTRACTS.md`, CHANGELOG.

## Приёмка
- Общие наборы `tp_testkit::suite` для новых трейтов проходят на in-memory (на PostgreSQL — META-70).
