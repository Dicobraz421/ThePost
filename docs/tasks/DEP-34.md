# DEP-34: пресет edge по SRS 2.6: без BLOB и очереди

lane: deploy
deps: [SYS-12]
class: A
files: [deploy/config/*, docs/manual-tests/SYS-12.md]
read: [docs/spec/S2.md#2.6, docs/manual-tests/SYS-12.md]
checks: [none]
manual: no

Хвост волны sys-07 (SYS-12), PR #400: `deploy/config/edge.yaml` (роль edge с локальным BLOB) под ограничениями не стартует —
seccomp запрещает `mkdir` каталога `.tmp`, Landlock — запись в каталог BLOB. По SRS 2.6 у edge нет BLOB и очереди,
профиль tp-sys прав на данные не даёт — расходится пресет.

## Сделать
- `edge.yaml`: без `blob`, без «локальной очереди» в комментарии; сквозная передача на transport (SRS 2.6). Если реплике
  получателей нужен каталог состояния — карточка полосы sys на профиль edge.
- Проверка запуска `edge.yaml` под `seccomp: enforce` и Landlock — строка в `docs/manual-tests/SYS-12.md`.

## Приёмка
- `thepost config check deploy/config/edge.yaml` без ошибок; узел edge стартует под ограничениями.
