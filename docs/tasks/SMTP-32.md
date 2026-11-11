# SMTP-32: отправка автоответа

lane: smtp
deps: [SMTP-31]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/spec/S4.md#4.3, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `RulesOutcome::vacation` → письмо `mail-builder` (RFC 3834: `MAIL FROM:<>`, `Auto-Submitted: auto-replied`, `In-Reply-To`,
  `References`) в очередь `Outbound`, локальному отправителю — сразу `ingest`; ограничение частоты на учётную запись (`delivery.vacation_rate`).
- Журнал доставки: событие «автоответ».

## Приёмка
- Тесты: ответ внешнему отправителю в очередь, внутреннему — в ящик, нет ответа на свой автоответ (петля).
