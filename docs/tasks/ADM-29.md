# ADM-29: ключ TLS экспорта аудита в syslog из хранилища секретов

lane: admin
deps: [CTR-52]
class: A
files: [crates/tp-admin/src/*]
read: [docs/tasks/CTR-52.md, docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 42: contracts-37, PR #480. `admin.audit.syslog` читает ключ только из файла (`export.rs`).

## Сделать
- `export.rs`: ключ клиентского сертификата syslog — только `key` (секрет); `key_file` удалить (решение 100, аудит 12.11).

## Приёмка
- Тест: экспорт аудита по TLS с ключом из хранилища в памяти.
