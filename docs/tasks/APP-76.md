# APP-76: узел — политика доверия в точках решения (SMTP, DAV, JMAP, Sieve, Admin API)

lane: app
deps: [META-85, ADM-49, SMTP-50, DAV-44, JMAP-47, SIEVE-14]
class: A
files: [crates/thepost/src/*, crates/thepost/tests/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#124, docs/tasks/CTR-93.md, docs/tasks/SMTP-50.md, docs/tasks/DAV-44.md]
checks: [nextest]
manual: no

## Сделать
- Узел получает `TrustStore` — `PgMeta` — и передаёт его в доставку SMTP, приём iTIP, занятость и общий доступ DAV,
  JMAP, правила Sieve и Admin API; память и `TrustMatrix::default()` в узле не остаются.
- Кэш действующей матрицы по учётной записи — с границей в секции конфигурации (SRS 5.5), сброс по событию
  `trust_changed` шины; ошибка чтения — `TrustMatrix::closed()` и метрика.
- Скрипт организации Sieve — до скриптов пользователей (этап before), сбой — временный отказ (SMTP-50).

## Как делают другие
- Политика применяется до пользовательских правил: Dovecot `sieve_before`
  (https://doc.dovecot.org/main/core/plugins/sieve.html); Exchange — правила потока до правил ящика
  (https://learn.microsoft.com/en-us/exchange/security-and-compliance/mail-flow-rules/mail-flow-rules).

## Приёмка
- e2e узла: `REQUEST` партнёра — письмо без события; администратор открывает `invite_new.partner = auto` — событие
  появляется у следующего; бронирование от партнёра — без брони; `REPLY` внешнего участника к событию нашего
  организатора — ответ учтён; внешнее письмо в закрытую группу — `550 5.7.1`; `redirect` наружу — не уходит.
- Изменение политики через Admin API действует без перезапуска (событие `trust_changed`); после перезапуска — на месте.
- Политика недоступна (имитация ошибки хранилища) — закрытая матрица, почта доставляется.
