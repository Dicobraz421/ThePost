# APP-10: подключение после app-02 — корзина, сжатие, нагрузка

lane: app
deps: [APP-04, IMAP-14, JMAP-11, MS-15]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-01-trash-purge.md]
checks: [nextest]
manual: no

## Сделать
- `imap::Deps` `trash`, `hooks` (#181); `jmap::Deps` `trash`, `policies` (#183); `Store::with_writer` (MS-15, #182), в `jmap::Deps::blobs` — хранилище без `Decompressing`.
- Истечение корзины (`TrashStore::expired` → точка принятия решений → `expunge_final`) — фоновая задача роли.
- `NodeLoad::inputs` (диски кэша — `LoadInputs::set_cache_disk`).

## Приёмка
- Сквозной тест MVP: «очистить корзину» в IMAP и JMAP по настройке; письма хранятся сжатыми.
