# APP-16: сокеты systemd, политики доменов в очереди, согласование doctor

lane: app
deps: [SYS-10, SMTP-15]
class: A
files: [crates/thepost/src/*]
read: []
checks: [nextest]
manual: no

Отчёты deploy-05 (DEP-08) и smtp-04 (SMTP-15).

## Сделать
- Активация сокетов: `listen_fds` → `InheritedSockets::adopt` через функцию SYS-10, снять `TODO(PLT-16)`; неиспользованные
  дескрипторы — `warn`.
- Очередь: `QueueWorker::with_routes(Arc::new(Routes::new(store)), Some(bus))` в `queue_worker` — без этого SMTP-15 не действует.
- `admin.listen` проверяется, а слушает `http.admin.listen` — один ключ (по SRS), второй — ошибка разбора с подсказкой.
- doctor: Landlock в контейнерах (`/sys/kernel/security` нет) — проба `landlock_create_ruleset` вместо чтения `lsm`;
  режим seccomp без секции `confine` — как в serve (`log`), а не `enforce`.
