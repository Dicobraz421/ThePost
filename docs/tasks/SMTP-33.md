# SMTP-33: приём iMIP — приглашения и ответы извне

lane: smtp
deps: [CTR-47, SMTP-31]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Письмо с частью `text/calendar` и `METHOD` (RFC 6047) локальному получателю — после доставки `ItipDelivery::deliver`
  (`Delivery::with_itip`); письмо остаётся в ящике; ошибка iTIP — запись в журнал, на доставку не влияет.
- Проверка отправителя: `ORGANIZER`/`ATTENDEE` должен совпадать с отправителем, прошедшим DMARC, иначе `Ignored`.

## Приёмка
- Тесты с заглушкой `ItipDelivery`: `REQUEST`, `REPLY`, `CANCEL`; письмо без iTIP не вызывает; подделанный отправитель — `Ignored`.
