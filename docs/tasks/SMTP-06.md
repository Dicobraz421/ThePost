# SMTP-06: Очередь исходящей почты

lane: smtp
deps: [SMTP-02, META-09]
class: A
files: [crates/tp-smtp/src/queue.rs]
read: [docs/spec/S4.md#4.1, docs/CONTRACTS.md#Queue (Ф1)]
checks: [nextest]
manual: no

## Сделать
- Постановка внешних получателей, роль smtp-out: claim с лизой, расписание повторов из конфигурации, статусы.

## Приёмка
- Тест: повторы по ManualClock, завершение при успехе.
