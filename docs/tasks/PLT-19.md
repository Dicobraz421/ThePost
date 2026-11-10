# PLT-19: события файловой системы через `notify`

lane: platform
deps: [PLT-17]
class: A
files: [crates/tp-platform/src/*]
read: []
checks: [nextest]
manual: no

Отчёт platform-07 (PLT-17): `os_events()` возвращает `NoFsEvents` — в tp-platform не было источника inotify без `unsafe`.
Зависимость `notify` — PR `x/platform-dep` (#209).

## Сделать
- `os_events()` на `notify` (inotify на Linux): каталоги конфигурации и TLS, подмена ConfigMap через `..data`.
- Ошибка подписки (лимит `max_user_watches`) — `warn` и опрос по интервалу, как сейчас.
- Тест на временном каталоге: запись файла → событие перечитывания не позже заданного срока.
