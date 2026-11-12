# SMTP-40: политика организации в SMTP вместо настроек config.yaml

lane: smtp
deps: [CTR-67, DEP-43]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/tasks/CTR-67.md]
checks: [nextest]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 5; решение 97.

## Сделать
- Через `PolicyResolver` (цепочка до учётной записи): `max_recipients` и `send_per_hour` — лимиты submission;
  `dkim_selector` — селектор подписи домена (ключи — по-прежнему ссылки на секреты); `milter_profile` — набор фильтров
  Milter из конфигурации (адреса фильтров остаются в config.yaml).
- Из `smtp` config.yaml удалить `limits.max_recipients`, `limits.messages_per_hour`, `limits.recipients_per_hour`,
  `dkim.domains.*.selector` — значения по умолчанию даёт политика организации.

## Приёмка
- Тесты: политика учётной записи с `max_recipients = 2` — третий получатель отклонён; селектор из политики подписывает;
  профиль Milter из политики вызывает свой набор.
