# DEP-07: OpenBao в эталонной поставке, роль приложения без суперпользователя

lane: deploy
deps: [DEP-06]
class: A
files: [deploy/*]
read: [docs/decisions/2026-10-30-config-events-secrets.md#Секреты — только через хранилище секретов, docs/decisions/2026-10-30-config-events-secrets.md#Права роли приложения в doctor (#44, вариант 1)]
checks: [none]
manual: docs/manual-tests/DEP-07.md

## Сделать
- compose: профиль dev — OpenBao в dev-режиме с секретами стенда; профиль по умолчанию — хранилище Raft, TLS, политики
  `thepost`, вход AppRole; Helm — чарт OpenBao и Kubernetes auth; Ansible — роль OpenBao.
- Распечатывание после перезапуска — по ответу владельца (по умолчанию автоматическое: PKCS#11/TPM или KMS, иначе ключ в файле).
- PostgreSQL: роль `thepost` без SUPERUSER и BYPASSRLS, её пароль — в OpenBao; миграции — той же ролью-владельцем схемы.

## Приёмка
- `docker compose up` → `thepost doctor` без `Fail`: RLS-роль, хранилище секретов, секретов в config.yaml нет.
