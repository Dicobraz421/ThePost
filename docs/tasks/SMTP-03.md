# SMTP-03: Локальная доставка

lane: smtp
deps: [SMTP-01, MS-02]
class: A
files: [crates/tp-smtp/src/deliver.rs]
read: [docs/spec/S4.md#4.1, docs/CONTRACTS.md#MailStore]
checks: [nextest]
manual: no

## Сделать
- Разрешение получателей (учётки, алиасы), Received/Return-Path, MailStore.ingest одним вызовом на письмо.

## Приёмка
- Тест на testkit: письмо нескольким получателям.
