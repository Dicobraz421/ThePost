# DAV-04: синхронизация — sync-collection и getctag

lane: dav
deps: [DAV-03]
class: A
files: [crates/tp-dav/src/*]
read: [docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `REPORT sync-collection` (RFC 6578): `sync-token` как URI с `ModSeq`, `sync-level: 1`, `limit` (507 и продолжение),
  удалённые — `404` в ответе; устаревший токен (`Conflict` хранилища) — 403 `valid-sync-token`.
- `getctag` (CalendarServer) и `sync-token` в `PROPFIND` коллекции — старые клиенты (iOS, macOS) опрашивают по ним.

## Приёмка
- Тесты: начальная синхронизация, изменения, удаления, страницы по `limit`, устаревший токен.
