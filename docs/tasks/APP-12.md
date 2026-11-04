# APP-12: сборка: соответствие Exchange и doctor автонастройки

lane: app
deps: [META-33, AUTH-10, MS-17, SMTP-27, JMAP-18, IMAP-15, ADM-10]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-04-exchange-parity.md, docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- Подключить новые компоненты в роли; doctor проверяет записи SRV (RFC 6186, 6764) и `autoconfig.<домен>`.

## Приёмка
- Сквозной тест: отложенная отправка с отменой, доставка от заблокированного отправителя в «Спам», запись аудита при доступе представителя.
