# CTR-50: подключение отключённого ящика — опция «оставить» / «удалить» содержимое

lane: contracts
deps: [CTR-35]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- Операция подключения отключённого ящика к учётной записи получает режим `ExistingContent::{Keep, Delete}`
  (решение 44): `Keep` — письма ящика учётной записи переносятся в подключаемый (фоновая задача, папки — с суффиксом
  при совпадении имён), `Delete` — в корзину со сроком; без режима и при непустом ящике — ошибка с подсказкой.
- Реализация в памяти и общий набор тестов.

## Приёмка
- CI зелёный; `docs/CONTRACTS.md`.
