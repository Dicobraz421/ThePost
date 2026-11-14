# DAV-42: организатор письма извне с адресом организации — внешний для бронирования

lane: dav
deps: []
class: A
files: [crates/tp-dav/src/booking.rs, crates/tp-dav/src/tests/sharing.rs]
read: [docs/tasks/DAV-19.md, docs/tasks/SMTP-33.md]
checks: [nextest]
manual: no

## Сделать
- Аудит iMIP: `BookingDeps::decide` не учитывал `ItipSource`. Письмо извне (`ItipSource::Imip`) с `From` и
  `ORGANIZER` из доменов организации проходит DMARC и через сторонний сервис, разрешённый в SPF или DKIM домена;
  такой организатор находился `AccountStore::by_address` как сотрудник и получал бронь (`ACCEPTED`).
- Для `Imip` организатор правил бронирования — внешний (`BookingRequest::organizer = None`, отказ
  `DeclineReason::NotAllowed`): сотрудники бронируют изнутри — CalDAV, JMAP, почтовый клиент (`Internal`). Ответ
  ресурса по-прежнему идёт учётной записи с этим адресом в календарь (`Booking::organizer`), а не письмом наружу:
  `ItipSender` — только для внешних адресов.

## Приёмка
- Приглашение ресурсу письмом извне от адреса сотрудника — брони нет, письма iMIP нет; та же встреча через CalDAV —
  `ACCEPTED`.
