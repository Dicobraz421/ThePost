# SYS-16: Landlock — кэш повторов Kerberos для входа GSSAPI

lane: sys
deps: []
class: A
files: [crates/tp-sys/src/*]
read: [docs/tasks/APP-58.md, docs/tasks/DEP-39.md]
checks: [nextest]
manual: yes

## Сделать
- MIT Kerberos при приёме контекста GSSAPI пишет кэш повторов в `/var/tmp` (`KRB5RCACHEDIR`), а Landlock узла туда
  не пускает — первый вход по GSSAPI упадёт (отчёт deploy-29, #708).
- Разрешить узлу каталог кэша повторов только при включённой `auth.gssapi`: свой каталог в `/var/lib/thepost/krb5rcache`
  (задать `KRB5RCACHEDIR` при запуске), правило Landlock — на него, а не на весь `/var/tmp`.

## Приёмка
- Тест: при `auth.gssapi` правило Landlock содержит каталог кэша, без неё — нет.
- Ручная проверка на стенде с KDC: вход `AUTHENTICATE GSSAPI` в IMAP проходит, в журнале нет отказа Landlock.
