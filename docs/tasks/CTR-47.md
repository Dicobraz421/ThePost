# CTR-47: планирование встреч — iTIP и занятость

lane: contracts
deps: [CTR-45]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `ItipDelivery::deliver(ItipMessage { tenant, recipient: AccountScope, from, method, ics })` — приём iTIP (RFC 5546) для учётной записи:
  вызывает smtp при доставке iMIP (RFC 6047) и сама полоса dav при планировании внутри организации; результат — `Applied`, `Ignored(reason)`.
- `ItipSender` — отправка iTIP внешнему участнику письмом iMIP через `queue::Outbound` (отправитель — организатор или участник).
- `FreeBusy::query(tenant, accounts, span) -> Vec<(AccountId, Vec<BusyPeriod { start, end, kind: Busy|Tentative|Unavailable }>)>`;
  данные — индекс занятости объектов (`ObjectIndex` дополняется `busy: Vec<BusyPeriod>` до горизонта из конфигурации);
  `Principal/getAvailability` и VFREEBUSY строятся на нём; jmap-09 (`Availability/get`) — тоже.
- `DavStore`: входящие планирования (`schedule-inbox`): `inbox_put`, `inbox_list`, `inbox_delete`; `Schedule-Tag` объекта.
- In-memory реализации и наборы `suite::itip`, `suite::freebusy`.

## Приёмка
- Conformance на in-memory; CHANGELOG в CONTRACTS.md.
