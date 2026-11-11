# DEP-24: эталон на Debian 13 и Ubuntu 26.04

lane: deploy
deps: [DEP-17]
class: A
files: [deploy/ansible/*, deploy/docker/*, docs/PREREQUISITES.md, docs/INSTALL.md, docs/spec/S8.md, docs/manual-tests/DEP-24.md]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [none]
manual: yes

## Сделать
- Роли Ansible работают на Debian 13 и Ubuntu 26.04 (как на Debian 12 и Ubuntu 24.04); версии PostgreSQL дистрибутивов;
  основа — Debian 13 (решение 51).
- Выпуск собирается под glibc Debian 12: проверить, на чём собирается бинарный файл в CI и Dockerfile, и записать
  минимальную glibc в `docs/PREREQUISITES.md`; S8 и INSTALL — список ОС.

## Приёмка
- `smb.yml` на чистых Debian 13 и Ubuntu 26.04 — `failed=0`, `thepost doctor` без `FAIL` — `docs/manual-tests/DEP-24.md`.
