# DEP-01: Dockerfile и compose для разработки

lane: deploy
deps: []
class: A
files: [deploy/docker/*, deploy/compose/*]
read: [docs/spec/S1.md#1.2]
checks: [none]
manual: yes

## Сделать
- Multi-stage Dockerfile (distroless), compose: PostgreSQL 16, SeaweedFS (S3), Valkey, Rspamd — профили dev и smb.

## Приёмка
- `docker compose up` поднимает зависимости; ручная проверка — docs/manual-tests/DEP-01.md.
