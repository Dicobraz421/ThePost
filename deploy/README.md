# deploy — окружение разработчика и средства развёртывания

| Путь | Содержание |
|---|---|
| `docker/Dockerfile` | OCI-образ: сборка на `rust:<ver>-bookworm`, запуск на `distroless/cc-debian12:nonroot` |
| `compose/dev.yml` | Зависимости для разработки (профиль `dev`) и эталон SMB (профиль `smb`) |
| `systemd/` | Юнит `thepost.service` с hardening, пользователь `thepost` (sysusers) |
| `config/` | Примеры `config.yaml` для пресетов `all` (SMB), `mailbox` (Enterprise-ВМ), `edge` (DMZ) |

## Чистая машина
1. Linux x86_64/AArch64 или Windows + WSL2 Ubuntu (работать внутри WSL, репозиторий — в файловой системе WSL).
2. Docker Engine с Compose v2 (`docker compose version`) или Docker Desktop с интеграцией WSL.
3. Rust через rustup (тулчейн подхватится из `rust-toolchain.toml`), `cargo install cargo-nextest --locked`.
   Пакеты для сборки: `build-essential pkg-config`; клиенты для проверок (необязательно): `postgresql-client`, `awscli`.
4. `git clone https://github.com/Dicobraz421/ThePost && cd ThePost`.

## Зависимости (профиль dev)
Из корня репозитория:
```bash
docker compose -f deploy/compose/dev.yml up -d
docker compose -f deploy/compose/dev.yml ps        # postgres, seaweedfs, valkey — healthy
```
Профиль `dev` выбран по умолчанию в `compose/.env`. Остановить с удалением данных:
`docker compose -f deploy/compose/dev.yml down -v`.

| Сервис | Адрес с хоста | Учётные данные (только dev) |
|---|---|---|
| PostgreSQL 16 | `localhost:5432` | `postgres` / `postgres`, БД `postgres` |
| SeaweedFS S3 | `http://localhost:8333` | ключ `thepost`, секрет `thepost-dev-secret` (`compose/seaweedfs-s3.json`) |
| Valkey | `localhost:6379` | без пароля |
| Rspamd | `http://localhost:11333` (проверка), `http://localhost:11334` (веб) | пароль веб-интерфейса `thepost-dev` |

Занятый порт хоста переопределяется переменной: `TP_PG_PORT`, `TP_S3_PORT`, `TP_VALKEY_PORT`, `TP_RSPAMD_PORT`,
`TP_RSPAMD_UI_PORT` (например, `TP_PG_PORT=55432 docker compose -f deploy/compose/dev.yml up -d`).

## Интеграционные тесты
Тест с внешней зависимостью пропускается, если её переменная не задана (`tp_testkit::pg_url()` и аналоги).
```bash
export TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres
export TEST_S3_URL=http://localhost:8333
export AWS_ACCESS_KEY_ID=thepost AWS_SECRET_ACCESS_KEY=thepost-dev-secret AWS_REGION=us-east-1
export TEST_VALKEY_URL=redis://localhost:6379
# TEST_LDAP_URL — каталога в compose пока нет; тесты LDAP пропускаются.
```
Прогон (как в конце волны, `docs/DEV.md`):
```bash
cargo nextest run -p <крейты> --no-tests=pass
```
Без переменных выполняются только тесты на in-memory реализациях `tp-testkit`.

## Эталон SMB в контейнерах (профиль smb)
`thepost` (`roles: [all]`, локальный BLOB, шина в процессе) + PostgreSQL + Rspamd; конфигурация — `compose/thepost-smb.yaml`.
```bash
docker compose -f deploy/compose/dev.yml --profile smb up -d --build
```
Порты 25, 465, 587, 143, 993, 80, 443 публикуются на хост; переопределение — `TP_SMTP_PORT`, `TP_SUBMISSIONS_PORT`,
`TP_SUBMISSION_PORT`, `TP_IMAP_PORT`, `TP_IMAPS_PORT`, `TP_HTTP_PORT`, `TP_HTTPS_PORT`.
Отдельная сборка образа: `docker build -f deploy/docker/Dockerfile -t thepost .` (`--build-arg RUST_VERSION=…`).

## Установка под systemd (SMB, Enterprise-ВМ)
```bash
sudo install -m 0755 target/release/thepost /usr/local/bin/thepost
sudo install -m 0644 deploy/systemd/thepost.sysusers.conf /usr/lib/sysusers.d/thepost.conf && sudo systemd-sysusers
sudo install -D -m 0640 -o root -g thepost deploy/config/all.yaml /etc/thepost/config.yaml   # задать domain, database.url
sudo install -m 0644 deploy/systemd/thepost.service /etc/systemd/system/thepost.service
sudo systemd-analyze verify /etc/systemd/system/thepost.service
sudo systemctl daemon-reload && sudo systemctl enable --now thepost
```
Данные — `/var/lib/thepost` (создаёт systemd). BLOB-каталог вне него — `systemctl edit thepost`:
`[Service]` + `ReadWritePaths=/srv/thepost-blob`. Проверка окружения — `thepost doctor` (SRS 5.5).
