# APP-45: межузловой канал: перечитывание сертификатов по событиям

lane: app
deps: [CLU-14]
class: A
files: [crates/thepost/src/*]
read: [docs/tasks/CLU-14.md, docs/tasks/PLT-17.md]
checks: [nextest]
manual: no

Хвост волны cluster-09 (CLU-14), PR #390: `NodeRpc::follow_tls` готов, но в `crates/thepost` не подключён — сертификаты
межузлового канала перечитываются только при перезапуске.

## Сделать
- `serve.rs`: `NodeRpc::follow_tls(&cluster.rpc.tls, events)` — события изменения каталога сертификатов (`watch` PLT-17),
  SIGHUP и перечитывание конфигурации (`reloader.subscribe()`), как у слушателей клиентов.

## Приёмка
- Интеграционный тест: замена файлов сертификата узла — новые соединения Rpc с новым сертификатом без перезапуска.
