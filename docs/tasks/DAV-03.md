# DAV-03: ресурсы — GET, PUT, DELETE, ETag, MKCOL и MKCALENDAR

lane: dav
deps: [DAV-02]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `GET`/`HEAD` объекта (`ETag`, `Content-Type`), `PUT` с `If-Match`/`If-None-Match: *` (412 при несовпадении), `DELETE` объекта и коллекции;
  ответ `PUT` — `ETag`, если тело сохранено без изменений (RFC 4791 §5.3.4).
- `MKCALENDAR` (RFC 4791 §5.3.1) и расширенный `MKCOL` (RFC 5689) с начальными свойствами; имя коллекции — сегмент URL.
- Индекс объекта (`ObjectIndex`) — модуль `tp_dav::index` поверх calcard: UID, компонент, интервал с развёрнутыми повторами до
  горизонта `dav.expand_horizon`; модулем пользуется и tp-jmap.

## Приёмка
- Тесты: создание, условная перезапись, конфликт ETag, удаление коллекции с объектами; индекс для повторяющегося события.
