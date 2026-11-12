# APP-55: узел: политика организации в SMTP

lane: app
deps: [SMTP-40]
class: A
files: [crates/thepost/src/*]
read: [docs/tasks/SMTP-40.md]
checks: [nextest]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 5; решение 97.

## Сделать
- `services.rs`: `PolicyResolver` (`PgMeta`) в зависимостях `tp-smtp` (submission, доставка, Milter, DKIM).

## Приёмка
- Интеграционный тест узла: политика организации, изменённая через Admin API, действует на отправку без перезапуска.
