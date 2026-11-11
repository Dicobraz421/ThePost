# META-48: журнал изменений DAV — токены синхронизации и надгробия

lane: meta
deps: [META-47]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `changes` по коллекции и `account_changes` по учётной записи: надгробия, постранично (`has_more`), устаревший токен — `Conflict`.
- Очистка надгробий старше срока (`meta.dav.tombstone_ttl`, по умолчанию 90 суток) — задание роли maintenance; после неё токен старше
  границы — `Conflict` (клиент делает полную синхронизацию, RFC 6578 `valid-sync-token`).

## Приёмка
- Тесты на PostgreSQL: страницы, удаление и повторное создание того же имени, токен до очистки — `Conflict`.
