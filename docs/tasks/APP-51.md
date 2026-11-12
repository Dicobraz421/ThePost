# APP-51: узел: книга «Организация» без LDAP

lane: app
deps: [META-65, APP-49]
class: A
files: [crates/thepost/src/*]
read: [docs/tasks/APP-49.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 43: app-28, PR #484.

## Сделать
- `services.rs`: без секции LDAP источник каталога для GAL (`router_with_gal`, `tp_dav::gal::sync` APP-49) — `PgMeta`
  (META-65); с LDAP — как сейчас.

## Приёмка
- Интеграционный тест узла без LDAP: учётная запись, созданная через Admin API, видна в книге «Организация» по CardDAV.
