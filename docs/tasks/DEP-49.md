# DEP-49: эталонные конфигурации: `maintenance.*` и `cli.oidc.*`

lane: deploy
deps: [APP-44, APP-39]
class: A
files: [deploy/*]
read: [docs/tasks/APP-44.md, docs/tasks/APP-39.md, docs/tasks/APP-42.md]
checks: [none]
manual: no

Хвост реестра облачных волн, п. 69 и 80: app-39, app-37, PR #530, #531.

## Сделать
- `deploy/config/*.yaml`, шаблон роли `thepost`, Helm: ключи `maintenance.verify.*`, `maintenance.admin_token_retention`,
  `cli.oidc.*` с умолчаниями и комментариями; `deploy/README.md`.

## Приёмка
- `thepost config check` для каждого файла без ошибок.
