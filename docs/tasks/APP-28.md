# APP-28: планирование встреч в узле

lane: app
deps: [DAV-16, SMTP-33, META-51, JMAP-31, APP-27]
class: A
files: [crates/thepost/src/*]
read: [docs/spec/S4.md#4.4]
checks: [nextest]
manual: docs/manual-tests/APP-28.md

## Сделать
- `ItipDelivery` из tp-dav в доставку smtp-in (`Delivery::with_itip`); `ItipSender` — через `Outbound` (очередь и DKIM); `FreeBusy` —
  `PgMeta` для DAV и JMAP.
- Ручная проверка: встреча внутри организации (Thunderbird ↔ iOS), приглашение внешнему участнику (Gmail или Outlook.com) и его ответ,
  занятость в Календаре macOS.

## Приёмка
- Интеграционный тест узла: приглашение внутреннему участнику и iMIP наружу в очереди; сценарии ручной проверки без замечаний.
