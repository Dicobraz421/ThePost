# IMAP-10: NAMESPACE, QUOTA, ACL

lane: imap
deps: [IMAP-03, AUTH-06, MS-06]
class: A
files: [crates/tp-imap/src/ext.rs]
read: [docs/spec/S4.md#4.2, docs/spec/S4.md#4.5]
checks: [nextest]
manual: no

## Сделать
- NAMESPACE (общие ящики), GETQUOTA/GETQUOTAROOT, ACL RFC 4314 (GETACL, SETACL, MYRIGHTS).

## Приёмка
- Тесты на testkit.
