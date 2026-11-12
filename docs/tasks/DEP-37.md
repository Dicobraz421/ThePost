# DEP-37: примеры секций `maintenance` и `dav` в `deploy/config`

lane: deploy
deps: [APP-32, APP-29]
class: A
files: [deploy/config/*, deploy/ansible/roles/thepost/*, deploy/helm/thepost/*]
read: [docs/tasks/APP-29.md, docs/tasks/APP-32.md]
checks: [none]
manual: no

Хвост реестра облачных волн, п. 37: app-31, PR #455. и п. 43 (app-28, PR #484): в `deploy/config/*.yaml` нет секции `maintenance` и ключей
`dav`, `http.dav`.

## Сделать
- `deploy/config/*.yaml`: пример `maintenance` (сроки очистки) и `dav` (`http.dav` или общий порт с JMAP, путь `/dav/`);
  шаблон config.yaml роли `thepost` и значения Helm — те же ключи с умолчаниями.

## Приёмка
- `thepost config check` для каждого файла `deploy/config/*.yaml` без ошибок; `helm template` без ошибок.
