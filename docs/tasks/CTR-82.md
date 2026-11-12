# CTR-82: вошедший пользователь в `Outbound::submit` — права отправителя через группы

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/delivery.rs, docs/CONTRACTS.md]
read: [docs/tasks/SMTP-36.md]
checks: [nextest]
manual: no

## Сделать
- В `Delivery::submit` сейчас не передаётся вошедший пользователь: права Send As / Send on Behalf проверяются по учётной
  записи отправителя конверта без групп, право, выданное группе, не распознаётся (отчёт smtp-16, #633).
- Добавить вошедшего пользователя в `Outbound::submit`; все реализации и вызовы (JMAP, отложенная отправка, DAV) —
  в том же PR, без значения по умолчанию.

## Приёмка
- Тест: право Send on Behalf, выданное группе, даёт отправку приглашения через JMAP члену группы.
