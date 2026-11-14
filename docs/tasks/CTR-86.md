# CTR-86: хранилище уведомлений о доступе (`ShareNotification`) в контракте

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/sharing.rs, docs/CONTRACTS.md]
read: [docs/tasks/JMAP-32.md]
checks: [nextest]
manual: no

## Сделать
- Хвост jmap-19 (#737): `ShareNotification` хранятся в памяти узла — не переживают перезапуск и не видны на других
  узлах.
- Контракт `ShareNotificationStore`: запись уведомления при изменении прав на коллекцию, список и изменения по состоянию
  (для `/get`, `/changes`, `/query`), удаление (`/set destroy`), срок хранения; тест набора в `tp-testkit`.
- Реализация в `tp-meta` (миграция — номер у оркестратора) и перевод JMAP с памяти на контракт — отдельными волнами.

## Приёмка
- Тест набора: уведомление видно получателю, `changes` по состоянию, удалённое не возвращается, чужой ящик — пусто.
