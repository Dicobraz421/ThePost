# DOC-05: справочник администратора: `maintenance.*` и `cli.oidc.*`

lane: docs
deps: [DEP-49]
class: A
files: [docs/INSTALL.md, docs/PREREQUISITES.md]
read: [docs/tasks/DEP-49.md]
checks: [none]
manual: no

Хвост реестра облачных волн, п. 69 и 80: app-39, app-37, PR #530, #531.

## Сделать
- `docs/INSTALL.md`: проверка целостности по расписанию, срок токенов `admin init`, вход CLI через OIDC — ключи и
  умолчания.

## Приёмка
- Описание совпадает с `thepost config schema`.
