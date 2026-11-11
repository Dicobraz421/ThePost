# DAV-14: iMIP наружу — приглашения внешним участникам

lane: dav
deps: [DAV-13]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Внешний участник (адрес вне принятых доменов) — письмо iMIP (RFC 6047) через `ItipSender`: `multipart/alternative` с текстом и
  `text/calendar; method=…`, вложение `.ics`, отправитель — организатор (подпись DKIM — в очереди, как у обычной отправки).
- Ответ участника наружу организатору извне — `REPLY` тем же путём.

## Приёмка
- Снимки писем iMIP (`insta`) для `REQUEST`, `CANCEL`, `REPLY`; письма проходят разбор mail-parser и calcard.
