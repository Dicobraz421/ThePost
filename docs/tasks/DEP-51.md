# DEP-51: эталоны deploy/config: edge.yaml и порты IMAP, ManageSieve

lane: deploy
deps: [DEP-37]
class: A
files: [deploy/config/*, crates/thepost/tests/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- `edge.yaml` проходит `thepost config check` (сейчас требует `database.url`): роль edge без базы — либо пример
  дополнить, либо ошибка проверки — по коду проверки, без второго варианта конфигурации.
- В эталонах — слушатели IMAP (143/993) и ManageSieve (4190), как в шаблоне Ansible; e2e узла, который читает
  `all.yaml`, не должен ломаться от второй секции (отчёт deploy-28).

## Приёмка
- `thepost config check` без ошибок на каждом `deploy/config/*.yaml`; e2e узла зелёный.
