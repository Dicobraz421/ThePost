# JMAP-46: `ShareNotification` из `ShareNotificationStore`, а не из памяти узла

lane: jmap
deps: [CTR-86, META-83]
class: B
files: [crates/tp-jmap/src/sharing.rs, crates/tp-jmap/src/core/mod.rs, crates/tp-jmap/src/tests/*, crates/thepost/src/services.rs]
read: [docs/tasks/META-83.md, docs/tasks/JMAP-32.md, crates/tp-contracts/src/sharing.rs]
checks: [nextest]
manual: no

## Сделать
- Структура `Notices` в `tp-jmap` (`Mutex<HashMap<..>>`, `MAX_NOTICES` в памяти узла) заменяется на
  `ShareNotificationStore`: поле `notifications` в `SharingDeps`; `/get`, `/changes`, `/query` и `/set` (`destroy`)
  читают и пишут через хранилище. Старая память удаляется, оговорка «хранятся в памяти узла» — из описания модуля.
- Межполосный PR (класс B): `SharingDeps` получает обязательное поле, поэтому сборку в `crates/thepost/src/services.rs`
  правим здесь же: `notifications` — `PgMeta`. Тесты `tp-jmap` — на памяти `tp-testkit`.
- Вытеснение сверх предела — CTR-90, не здесь.

## Приёмка
- Тесты `tp-jmap` проходят на хранилище из `tp-testkit`; структуры `Notices` в коде нет; узел собирается.
