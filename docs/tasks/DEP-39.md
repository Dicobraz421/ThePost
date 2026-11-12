# DEP-39: образ и эталоны: libkrb5 для GSSAPI

lane: deploy
deps: [DEPS-12]
class: A
files: [deploy/*]
read: [docs/tasks/DEPS-12.md]
checks: [none]
manual: no

Хвост реестра облачных волн, п. 54: app-26, PR #491. и п. 39 (auth-08, PR #478): в keytab нужен принципал `sieve/<узел>`.

## Сделать
- `deploy/docker/Dockerfile`: библиотека `libkrb5` в образе; роль `thepost`: пакет и путь keytab (`KRB5_KTNAME`);
  `deploy/README.md`: принципалы `smtp/`, `imap/`, `HTTP/`, `sieve/<узел>` в keytab.

## Приёмка
- `docker build` и `--syntax-check` без ошибок.
