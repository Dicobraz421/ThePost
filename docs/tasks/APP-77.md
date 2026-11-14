# APP-77: узел — ответ на наше письмо и беседы: ключи меток, `SentLog`, `ThreadStore`

lane: app
deps: [META-86, MS-21, SMTP-51, JMAP-48, ADM-50]
class: A
files: [crates/thepost/src/*, crates/thepost/tests/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#125, docs/tasks/CTR-94.md, docs/tasks/SMTP-51.md]
checks: [nextest]
manual: no

## Сделать
- Узел получает `SentLog`, `ThreadStore` и политики беседы — `PgMeta` — и передаёт их доставке SMTP, `MailStore`, JMAP
  и Admin API; памяти в узле не остаётся.
- Ключи метки Message-ID и BATV — из хранилища секретов (OpenBao) по `SecretRef` с id; смена ключа — без перезапуска,
  прежний проверяет до конца срока меток. Нет ключа — метки не ставятся и не проверяются (признак ответа — только по
  хэшу), BATV в `log`; предупреждение в `thepost doctor`.
- Очистка `SentLog` старше `smtp.reply.max_age` — роль maintenance; фоновая привязка старых писем к беседам (META-86).

## Как делают другие
- Ключи с id и ротацией — как ключи DKIM узла и cookie Rspamd `replies` (`cookie_key`, https://docs.rspamd.com/modules/replies).

## Приёмка
- e2e узла: наше письмо наружу с меткой и BATV → ответ адресата (DMARC pass) получает `X-ThePost-Reply`; bounce на
  адрес без метки — `550 5.7.1`; ответ попадает в беседу исходного письма, удержание беседы переживает перезапуск.
- Смена ключа метки: ответ на письмо со старым ключом ещё признаётся, на новое — с новым.
