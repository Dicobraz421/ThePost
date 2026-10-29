# SMTP-08: DKIM-подпись

lane: smtp
deps: [SMTP-06]
class: A
files: [crates/tp-smtp/src/dkim_sign.rs]
read: [docs/spec/S4.md#4.1]
checks: [nextest]
manual: no

## Сделать
- Подпись исходящих (RSA-SHA256, Ed25519), ключи по домену из конфигурации, ротация селекторов.

## Приёмка
- Подпись проверяется mail-auth в тесте.
