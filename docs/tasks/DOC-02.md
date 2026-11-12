# DOC-02: INSTALL: новый сервер без ожидания готовности узла

lane: docs
deps: [DEP-27]
class: A
files: [docs/INSTALL.md]
read: [docs/manual-tests/DEP-23.md]
checks: [none]
manual: no

Хвост волны deploy-19 (DEP-23), PR #421 и #425.

## Сделать
- `docs/INSTALL.md`, «Новый сервер вместо потерянного», шаг 1: `smb.yml -e thepost_backup=false -e thepost_wait_ready=false`
  вместо «прогон остановится на `Wait for thepost readiness` — это ожидаемо».

## Приёмка
- Шаги совпадают с `docs/manual-tests/DEP-23.md` после DEP-27.
