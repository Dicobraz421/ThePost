# JMAP-36: Calendar и AddressBook — `isDefault`, `isSubscribed`, `isVisible`, оповещения по умолчанию

lane: jmap
deps: [CTR-56, JMAP-27, JMAP-25]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md#Dav]
checks: [nextest]
manual: no

Хвост волны jmap-17, PR #444 (книга по умолчанию — волна jmap-16, PR #428).

## Сделать
- `Calendar/get`, `/set`: `isSubscribed`, `isVisible`, `defaultAlertsWithTime`, `defaultAlertsWithoutTime` читаются и
  пишутся через поля `Collection` (CTR-56) — сейчас всегда `true` и `null`; `onSuccessSetIsDefault` назначает
  календарь по умолчанию; `useDefaultAlerts` события берёт оповещения календаря.
- `AddressBook/set`: `onSuccessSetIsDefault` для другой книги — сейчас `invalidArguments`; `isSubscribed`.

## Приёмка
- Тесты: смена календаря и книги по умолчанию; скрытие календаря; событие с `useDefaultAlerts` получает оповещения
  календаря.
