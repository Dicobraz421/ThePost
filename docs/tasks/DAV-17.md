# DAV-17: общий доступ к календарям и адресным книгам

lane: dav
deps: [DAV-16, CTR-48, AUTH-12]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/spec/S4.md#4.5, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Права на коллекцию (`CollectionAcl`) через `Authorizer`: чтение, запись, только занятость; общие коллекции — в доме получателя
  (`calendar-home-set`) ссылками; `current-user-privilege-set` по действующим правам.
- Выдача прав — `POST` CalendarServer sharing (`share-resource`) для Календаря macOS и iOS; другие клиенты — через JMAP `shareWith` и Admin API.

## Приёмка
- Тесты: чтение без записи (403), запись, отзыв; общий календарь виден в доме получателя.
