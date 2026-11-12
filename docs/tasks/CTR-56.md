# CTR-56: `Collection` — по умолчанию, подписка и видимость, оповещения по умолчанию

lane: contracts
deps: [CTR-45]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md#Dav, docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

Хвост волны jmap-17, PR #444 (для адресных книг — волны jmap-16, PR #428).

## Сделать
- `Collection` (календарь и адресная книга), все поля — `#[serde(default)]`: признак коллекции по умолчанию (одна на
  вид коллекции у учётной записи; назначение новой снимает признак с прежней одной операцией), подписка и видимость
  (`isSubscribed`, `isVisible` JMAP), оповещения календаря по умолчанию для событий со временем и на весь день
  (`defaultAlertsWithTime`, `defaultAlertsWithoutTime`; в CalDAV — `default-alarm-*`), с пределом размера.
- Без назначенной коллекции по умолчанию — прежнее правило (сегмент `default`, иначе первая по порядку).
- Реализация в памяти, общий набор тестов, `docs/CONTRACTS.md#Dav`, CHANGELOG. Потребители: META-60, DAV-25, JMAP-36.

## Приёмка
- CI зелёный; тесты набора: смена коллекции по умолчанию — признак ровно у одной; оповещения сохраняются и читаются.
