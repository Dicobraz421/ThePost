# APP-71: узел — общий доступ, представители и бронирование CalDAV (хвост dav-05)

lane: app
deps: [DAV-17, DAV-18, DAV-19]
class: A
files: [crates/thepost/src/*]
read: [docs/tasks/DAV-17.md, docs/tasks/DAV-18.md, docs/tasks/DAV-19.md]
checks: [nextest]
manual: yes

## Сделать
- Отчёт волны dav-05 (#756): без подключения в узле не работают выдача прав через CalDAV `POST` (`cs:share`,
  `share-resource`), состав групп представителей (`calendar-proxy`) и бронирование переговорных.
- Вызов `tp_dav::router_with` заменить на
  `router_shared(deps, schedule, Some(tp_dav::SharingStores { acl, delegates }), cfg)`; `acl` и `delegates` — `PgMeta`.
  Старый вызов удаляется.
- `ItipInbox::with_booking(BookingDeps { accounts, resources, freebusy, group_store, sender })`: хранилища — `PgMeta`,
  `sender` — `ImipSender` узла.

## Приёмка
- e2e узла: выдача и отзыв прав через `POST` на коллекцию; `PROPPATCH group-member-set` назначает представителя;
  приглашение переговорной — бронь `ACCEPTED` в её календаре и `REPLY` организатору.
- Ручная проверка — `docs/manual-tests/DAV-19.md` на стенде (macOS и Thunderbird).
