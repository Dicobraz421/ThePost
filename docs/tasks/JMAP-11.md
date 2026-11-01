# JMAP-11: Email/set destroy в корзине — purge; Email/import — append

lane: jmap
deps: [CTR-26, CTR-27, JMAP-05]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/decisions/2026-11-01-trash-purge.md]
checks: [nextest]
manual: no

## Сделать
- `Email/set destroy` в корзине → `purge` (вместо `notDestroyed`); `Email/import` → `MailStore::append`; настройка пользователя `trash.purge` — в его настройках, если `user_can_choose`.

## Приёмка
- Тесты: destroy в корзине скрывает/стирает по настройке; import не вызывает точки принятия решений доставки.
