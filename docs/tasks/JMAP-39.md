# JMAP-39: JMAP Contacts: книга GAL

lane: jmap
deps: [CTR-60, JMAP-34]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

Синк 12.11, часть 2, решение 72 (часть JMAP-26, не сделанная в jmap-16, PR #428).

## Сделать
- `AddressBook/get` отдаёт книгу GAL организации (`isSubscribed`, права только чтения); `ContactCard/get`, `/query`,
  `/changes` — по ней из `DavStore`; `ContactCard/set` в неё — `forbidden`.

## Приёмка
- Тесты на in-memory: книга видна, карточки читаются и ищутся, изменение — `forbidden`, `changes` после `sync_gal`.
