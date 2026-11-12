# APP-32: очистка надгробий DAV и журнала автоответов в роли maintenance

lane: app
deps: [META-48, META-50]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- Шаги `purge_dav_tombstones` и `purge_vacation_log` в существующей роли maintenance рядом с корзиной
  (решение 65); сроки — из `meta`.

## Приёмка
- Тест: после шага старые записи удалены, свежие на месте.
