# AUTH-17: ключ TLS клиентов LDAP и OIDC из хранилища секретов

lane: auth
deps: [CTR-52]
class: A
files: [crates/tp-auth/src/*]
read: [docs/tasks/CTR-52.md, docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 42: contracts-37, PR #480. поле `key` у TLS LDAP и OIDC (`tp-auth`) не читается — для этих каналов ключ
из OpenBao недоступен (решение 69).

## Сделать
- `ldap.rs`, `oidc.rs`: ключ клиентского сертификата — только `key` (секрет); `key_file` удалить (решение 100,
  аудит 12.11).

## Приёмка
- Тесты: ключ из хранилища в памяти даёт ту же идентичность клиента, что из файла; без хранилища — понятная ошибка.
