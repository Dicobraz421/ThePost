# APP-39: maintenance: срок журнала доставки и токенов `admin init`

lane: app
deps: [META-22, META-43]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Хвосты волн app-16 (PR #293: `DeliveryLog::purge` не подключён) и meta-27 (PR #295: периодический `prune` локальных
токенов администратора не подключён).

## Сделать
- Роль maintenance (`roles.rs`, рядом с истечением корзины) под лизой лидера: раз в сутки `PgMeta::purge_expired_delivery`
  (срок — `meta.delivery.retention`, 30 сут) и `LocalAdminTokens::prune(now − срок)` — удаляются токены `admin init`,
  истёкшие раньше срока (по умолчанию 30 сут; параметр секции `admin` или `meta`).
- Без этого журнал доставки и таблица токенов растут без предела.

## Приёмка
- Тест: после шага старые записи журнала доставки и истёкшие токены удалены, свежие на месте.
