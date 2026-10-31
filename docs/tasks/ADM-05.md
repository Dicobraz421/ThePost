# ADM-05: поиск в журнале доставки

lane: admin
deps: [ADM-01, META-22]
class: A
files: [crates/tp-admin/src/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

## Сделать
- Маршрут поиска по отправителю, получателю, Message-ID, интервалу; постраничная выдача; права — администратор организации и оператор только для чтения.

## Приёмка
- Тесты маршрута на фейках.
