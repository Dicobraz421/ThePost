# APP-68: узел: каталог кэша повторов Kerberos при входе GSSAPI

lane: app
deps: [SYS-16]
class: A
files: [crates/thepost/src/startup.rs]
read: [docs/tasks/SYS-16.md]
checks: [nextest]
manual: no

## Сделать
- Хвост SYS-16 (#722): в tp-sys есть `KRB5_RCACHE_DIR` (`/var/lib/thepost/krb5rcache`) и `RolePaths::krb5_rcache`,
  но узел поле не заполнял — правило Landlock на каталог не появлялось.
- `startup.rs`, `role_paths()`: при `auth.gssapi` — `paths.krb5_rcache = Some(tp_sys::KRB5_RCACHE_DIR.into())`;
  `create_role_dirs()` создаёт и этот каталог (0700) до наложения Landlock.
- Переменную `KRB5RCACHEDIR` узел не задаёт: её задаёт развёртывание (DEP-53).

## Приёмка
- Модульный тест `startup.rs`: при `auth.gssapi` каталог есть в `RolePaths`, без секции — нет; `create_role_dirs`
  создаёт его с правами 0700.
- Источник: хвост SYS-16 (#722).
