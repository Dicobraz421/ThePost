# DAV-18: представители — calendar-proxy

lane: dav
deps: [DAV-17]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/spec/S4.md#4.5]
checks: [nextest]
manual: no

## Сделать
- Принципалы `calendar-proxy-read` и `calendar-proxy-write` (CalendarServer), `calendar-proxy-read-for`/`-write-for`: представитель видит
  и правит календари владельца в iOS и macOS («Делегирование»).
- Ответы на приглашения от имени владельца (`SENT-BY`) — при `schedule_on_behalf`.

## Приёмка
- Тесты на in-memory; ручная проверка — делегирование в Календаре macOS (сценарий в DAV-19).
