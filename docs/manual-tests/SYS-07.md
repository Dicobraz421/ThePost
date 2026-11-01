# SYS-07: seccomp и Landlock по ролям — ручная проверка

Нужны: Linux-ВМ с Landlock (ABI v5 — полный набор прав; ABI v8 — применение ко всем потокам), сборка
`thepost`, в которой запуск применяет `tp_sys::Confinement` после инициализации ролей (PLT-16). До PLT-16
проверка откладывается: автоматические тесты SYS-07 проверяют ограничения на отдельном потоке.

## Узел edge
1. config.yaml: `roles: [edge]`, секция `confine` по умолчанию (`seccomp: enforce`, `landlock: true`).
2. Запустить `thepost serve`; в журнале — `ограничения ролей применены` без предупреждений
   (на ядре без Landlock — предупреждение `ядро не поддерживает Landlock…`, узел работает).
3. `grep -E 'Seccomp|NoNewPrivs' /proc/$(pidof thepost)/status` → `NoNewPrivs: 1`, `Seccomp: 2`, `Seccomp_filters: 2`.
4. Принять письмо через edge (`swaks --to user@example.org --server <edge>`): ответ 250 — сеть и чтение конфигурации работают.
5. `strace -f -p $(pidof thepost) -e trace=mkdir,mkdirat,unlinkat,execve` во время приёма: вызовов нет; при
   искусственном вызове (отладочная сборка) — `EPERM`.

## Роли с данными и indexer
1. `roles: [mailbox]`, BLOB на локальном диске, кэш на NVMe.
2. Приём и чтение писем по IMAP работают; извлечение текста вложений (indexer) — подпроцессы запускаются,
   документ проиндексирован.
3. Обращение процесса вне каталогов ролей (например, путь BLOB, не указанный в конфигурации) — отказ
   `EACCES`; на ядрах с Landlock ABI v7 — запись в журнале аудита (`journalctl -k | grep landlock`).

## Режим журнала
1. `confine: { seccomp: log }`; перезапуск — предупреждение `seccomp в режиме журнала…`.
2. `journalctl -k | grep 'type=1326'` — записи о вызовах вне профиля (если есть); узел работает.
   Найденные вызовы — в профиль (`crates/tp-sys/src/confine/seccomp.rs`) отдельной правкой.
