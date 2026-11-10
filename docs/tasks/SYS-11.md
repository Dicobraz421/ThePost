# SYS-11: проба Landlock без `/sys/kernel/security`

lane: sys
deps: [SYS-07]
class: A
files: [crates/tp-sys/src/*]
read: []
checks: [nextest]
manual: no

Отчёты deploy-05 и app-15: doctor читает Landlock из `/sys/kernel/security/lsm` — в контейнерах и подах каталога нет, WARN.

## Сделать
- `tp_sys::landlock_abi() -> Option<i32>`: `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`,
  `None` при ENOSYS/EOPNOTSUPP; `unsafe` с SAFETY. Тест: функция не паникует, на ядре с Landlock — `Some(>=1)`.
