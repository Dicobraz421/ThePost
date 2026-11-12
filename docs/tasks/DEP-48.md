# DEP-48: одна конфигурация SMB — из пресета

lane: deploy
deps: []
class: A
files: [deploy/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [none]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 19; решение 105.

## Сделать
- Конфигурация SMB сейчас в трёх копиях (`deploy/config/all.yaml`, `deploy/compose/thepost-smb.yaml`, шаблон роли
  `thepost`): один пресет `deploy/config/smb.yaml`, compose и роль строят config.yaml из него (роль — только
  подстановка значений инвентаря).

## Приёмка
- `thepost config check` для пресета; compose и Ansible дают одинаковые секции (сравнение в README или тесте роли).
