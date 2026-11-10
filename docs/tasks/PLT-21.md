# PLT-21: doctor — проба Landlock; подпись метрики отставания шины

lane: platform
deps: [SYS-11, CLU-13]
class: A
files: [crates/tp-platform/src/*]
read: []
checks: [nextest]
manual: no

Отчёты app-15 и cluster-08.

## Сделать
- `privileges::doctor`: Landlock — по `tp_sys::landlock_abi()` вместо чтения `/sys/kernel/security/lsm`.
- `tp_bus_lagged` (telemetry.rs): подпись — «сколько раз подписчики отставали и получали Resync».
