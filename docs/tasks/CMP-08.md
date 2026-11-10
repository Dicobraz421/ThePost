# CMP-08: `Outbound` JMAP со статусами по получателям

lane: jmap
deps: [CMP-03]
class: A
files: [crates/tp-jmap/src/*]
read: []
checks: [nextest]
manual: no

Отчёт smtp-08 (CMP-04): трейт `tp_jmap::Outbound` возвращает `Result<()>`, поэтому статусы доставки по получателям
(доставлено локально, в очереди, ошибка) не попадают в `deliveryStatus` EmailSubmission (RFC 8621 §7).

## Сделать
- Метод трейта со статусами по получателям (образец — `tp_smtp::Submitted`, но тип — свой в tp-jmap), с реализацией по
  умолчанию через старый метод (все получатели — `queued`), чтобы `thepost` собирался без правки (CMP-07 подключит).
- `EmailSubmission/set` и `get`: `deliveryStatus` — `smtpReply`, `delivered` (`yes`/`queued`/`no`), `displayed: unknown`.
- Тесты на фейках: смешанный список получателей → статусы по каждому.
