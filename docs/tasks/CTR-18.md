# CTR-18: окончательное удаление — только из корзины

lane: contracts
deps: [CTR-07]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md#Trash, Retention, Quota (Ф1)]
checks: [nextest]
manual: no

## Сделать
- Гонка (отчёт mailstore-02, PR #117): письмо, восстановленное между `TrashStore::expired` и `MessageStore::expunge_final`, удаляется.
- `expunge_final` удаляет только письма, которые всё ещё в корзине (условие в контракте и документации); остальные — пропуск, не ошибка. Реализации (tp-meta, tp-testkit) — в том же PR.

## Приёмка
- Тест контракта: восстановление между `expired` и `expunge_final` → письмо остаётся.
