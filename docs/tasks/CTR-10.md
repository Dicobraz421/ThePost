# CTR-10: Точки расширения и модули

lane: contracts
deps: [CTR-04, CTR-05]
class: B
files: [crates/tp-contracts/src/ext.rs]
read: [docs/spec/S2.md#2.5, docs/spec/S1.md#1.3, docs/CONTRACTS.md#Extension points]
checks: [nextest]
manual: no

## Сделать
- Трейт Module (имя, схема mod_<имя>, миграции, register); Registry: роли, capability JMAP, маршруты Admin API;
- DecisionHook: before_final_delete, on_delivery, on_submission → Allow | Deny(reason) | Augment; EventSink — подписчик журнала событий.

## Приёмка
- Трейты object-safe; пустой набор хуков возвращает Allow (doc-тест).
