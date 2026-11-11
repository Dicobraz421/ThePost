# SIEVE-04: действия — fileinto, copy, imap4flags, redirect, mailbox

lane: sieve
deps: [SIEVE-03]
class: A
files: [crates/tp-sieve/src/*]
read: [docs/spec/S4.md#4.3, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `fileinto` (RFC 5228 §4.1) с `:copy` (RFC 3894) и `:create` (RFC 5490), тест `mailboxexists`; имя папки — как в IMAP.
- `imap4flags` (RFC 5232): `setflag`, `addflag`, `removeflag`, тест `hasflag`, `:flags` у `keep` и `fileinto`.
- `redirect` с `:copy`; лимит адресов на письмо (`sieve.max_redirects`); защита от петли — по `Received` и метке узла, как у
  пересылки (SRS 4.1).
- Слияние действий в `RulesOutcome`: повтор папки — одна копия, `discard` отменяет implicit keep (RFC 5228 §2.10.1).

## Приёмка
- Тесты действий и их сочетаний; `fileinto` в несуществующую папку без `:create` — implicit keep и диагностика.
