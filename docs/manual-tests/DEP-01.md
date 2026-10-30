# DEP-01: Dockerfile и compose — ручная проверка

Linux или WSL2 с Docker Engine / Docker Desktop, Compose v2. Команды — из корня репозитория.

## Профиль dev (зависимости)
1. `docker compose -f deploy/compose/dev.yml up -d` → контейнеры postgres, seaweedfs, valkey, rspamd запущены.
2. `docker compose -f deploy/compose/dev.yml ps` → postgres, seaweedfs, valkey в состоянии `healthy` за ≤60 с.
3. `psql postgres://postgres:postgres@localhost:5432/postgres -c 'select version()'` → PostgreSQL 16.
4. `curl -s http://localhost:8333/healthz` → код 200; `aws --endpoint-url http://localhost:8333 s3 mb s3://tp-check`
   с `AWS_ACCESS_KEY_ID=thepost AWS_SECRET_ACCESS_KEY=thepost-dev-secret` → бакет создан.
5. `docker compose -f deploy/compose/dev.yml exec valkey valkey-cli ping` → `PONG`.
6. `curl -s http://localhost:11333/ping` → `pong`; http://localhost:11334 открывает веб-интерфейс Rspamd (пароль `thepost-dev`).
7. `docker compose -f deploy/compose/dev.yml down -v` → контейнеры и тома удалены.

## Образ и профиль smb
1. `docker build -f deploy/docker/Dockerfile -t thepost:dev .` → сборка успешна; `docker run --rm thepost:dev --version`
   (или без аргументов, пока нет CLI) печатает версию.
2. `docker image inspect thepost:dev --format '{{.Config.User}}'` → `65532:65532`.
3. `docker compose -f deploy/compose/dev.yml --profile smb up -d --build` → запущены только postgres, rspamd, thepost.
   До APP-01/PLT-01 (`thepost serve`) контейнер thepost завершается сразу — ожидаемо.
4. `docker compose -f deploy/compose/dev.yml --profile smb down -v`.
