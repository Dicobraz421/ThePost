# SMTP-36: приглашения из почтового клиента — в календарь коллеги

lane: smtp
deps: [SMTP-33, CTR-53]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

Синк 12.11, часть 2, решение 78 (хвост smtp-13, PR #447, решение 2).

## Сделать
- Письмо с частью `text/calendar; method=…` через submission и `Delivery::submit` (JMAP) локальному получателю — после
  доставки в `ItipDelivery` с `ItipSource::Internal`, если `From` письма принадлежит вошедшей учётной записи: свой адрес,
  псевдоним (CTR-53) или право Send As / Send on Behalf (`Authorizer`); иначе — только письмо, запись в `tracing`.
- Остальные проверки — как у iMIP (SMTP-33): `METHOD`, совпадение с `ORGANIZER` или `ATTENDEE`; внешним получателям —
  без изменений.

## Приёмка
- Тесты с заглушкой `ItipDelivery`: `REQUEST` из submission со своим `From` — доходит; с чужим `From` без права — нет;
  с правом Send As — доходит; письмо во «Входящих» при любом исходе.
