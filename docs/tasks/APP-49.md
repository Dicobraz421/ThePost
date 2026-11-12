# APP-49: узел: заполнение книги GAL

lane: app
deps: [DAV-29, META-63, JMAP-39]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Синк 12.11, часть 2, решение 72.

## Сделать
- Роль `dav`: `tp_dav::gal::sync` при старте и по событию изменения каталога (учётные записи, группы, синхронизация
  LDAP); при нескольких узлах — под лизой (одна запись на организацию).

## Приёмка
- Тест узла: новая учётная запись появляется в книге GAL по CardDAV и JMAP.
