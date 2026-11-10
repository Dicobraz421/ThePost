# SYS-09: ожидание событий PSI в tp-sys

lane: sys
deps: [PLT-17]
class: A
files: [crates/tp-sys/src/*]
read: []
checks: [nextest]
manual: no

Отчёт platform-07 (PLT-17): ожидание триггера PSI в tp-platform сделано через устаревший `AsyncFd::with_interest` под
`#[allow(deprecated)]` — замена (`register_with_interest`) требует `unsafe`, разрешённого только в tp-sys.

## Сделать
- В tp-sys: безопасная обёртка «триггер PSI» (запись порога, ожидание `POLLPRI`) для tokio; ядро без PSI
  (`CONFIG_PSI_DEFAULT_DISABLED`, `EINVAL`) — ошибка, по которой вызывающий переходит на интервал.
- tp-platform переводится на неё в своей следующей волне (перечислить место в отчёте).
