# ADM-36: Admin API: проверка полей политики для SMTP

lane: admin
deps: [CTR-67]
class: A
files: [crates/tp-admin/src/*]
read: [docs/tasks/CTR-67.md]
checks: [nextest]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 5; решение 97.

## Сделать
- `PUT …/policy`: `dkim_selector` — есть ключ с таким селектором для домена, `milter_profile` — профиль есть в
  конфигурации узла (иначе 422 с пояснением); лимиты — положительные; описание полей в OpenAPI.

## Приёмка
- Тесты: неизвестный селектор и профиль — 422.
