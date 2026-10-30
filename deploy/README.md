# deploy — окружение разработчика и средства развёртывания

| Путь | Содержание |
|---|---|
| `docker/Dockerfile` | OCI-образ: сборка на `rust:<ver>-bookworm`, запуск на `distroless/cc-debian12:nonroot` |
| `compose/dev.yml` | Зависимости: только TLS (профиль `tls`, по умолчанию) и открытые для тестов (`dev`); эталон SMB (`smb`) |
| `compose/tls/` | Генерация самоподписанного CA (`gen-certs.sh`), `pg_hba.conf` (только `hostssl`) |
| `compose/openbao/` | OpenBao в compose: конфигурация (Raft, TLS, seal `static`), запуск, начальная настройка (`init.sh`) |
| `compose/postgres/app-role.sh` | Роль приложения `thepost` без SUPERUSER и BYPASSRLS — владелец БД `thepost` |
| `openbao/thepost-policy.hcl` | Политика узла `thepost` в OpenBao (общая для compose, Helm, Ansible) |
| `helm/openbao/` | Значения чарта `openbao/openbao` и настройка Kubernetes auth (`bootstrap.sh`) |
| `ansible/` | Роль `openbao` и плейбук `openbao.yml` (ВМ: пакет, Raft, TLS, распечатывание, AppRole) |
| `systemd/` | Юнит `thepost.service` с hardening, пользователь `thepost` (sysusers) |
| `config/` | Примеры `config.yaml` для пресетов `all` (SMB), `mailbox` (Enterprise-ВМ), `edge` (DMZ) |

## Чистая машина
1. Linux x86_64/AArch64 или Windows + WSL2 Ubuntu (работать внутри WSL, репозиторий — в файловой системе WSL).
2. Docker Engine с Compose v2 (`docker compose version`) или Docker Desktop с интеграцией WSL.
3. Rust через rustup (тулчейн подхватится из `rust-toolchain.toml`), `cargo install cargo-nextest --locked`.
   Пакеты для сборки: `build-essential pkg-config`; клиенты для проверок (необязательно): `postgresql-client`, `awscli`.
4. `git clone https://github.com/Dicobraz421/ThePost && cd ThePost`.

## Зависимости
Внутренние каналы — только TLS (решение 2026-10-30); открытые каналы — только профиль `dev`, явной настройкой.
Секреты `thepost` — только из OpenBao (решение 2026-10-30): в `config.yaml` — ссылки `{ secret: "<путь>#<поле>" }`.
Профили `tls` и `dev` публикуют одни и те же порты хоста — запускается один из них.

### Профиль tls (по умолчанию)
```bash
docker compose -f deploy/compose/dev.yml up -d --wait   # tls-init, postgres-tls, seaweedfs-tls, valkey-tls, openbao-tls
```
Профиль `tls` выбран по умолчанию в `compose/.env`. Сервис `tls-init` при первом запуске создаёт самоподписанный CA
и серверный сертификат (SAN: `localhost`, `127.0.0.1`, имена сервисов) в томе `tls`; копия CA — `compose/tls/ca.pem`.
Новый CA — `down -v`; сменился список SAN — сертификат перевыпускается при `up`. Вне compose:
`sh deploy/compose/tls/gen-certs.sh <каталог>`.

| Сервис | Адрес с хоста | Подключение |
|---|---|---|
| PostgreSQL 16 | `localhost:5432` | только TLS (`pg_hba`: `hostnossl … reject`): `sslmode=verify-full&sslrootcert=deploy/compose/tls/ca.pem` |
| SeaweedFS S3 | `https://localhost:8333` | только https; CA — `compose/tls/ca.pem`; бакет `thepost-test` |
| Valkey | `rediss://localhost:6379` | только TLS (`--port 0`), сертификат клиента не нужен |
| OpenBao 2.7 | `https://localhost:8200` | только TLS; CA — `compose/tls/ca.pem`; вход узла — AppRole `thepost` |

OpenBao профилей `tls` и `smb` — хранилище Raft (том `openbao-data`), распечатывание после перезапуска —
автоматическое ключом из файла (seal `static`, том `openbao-seal`; на стенде нет TPM/HSM и KMS, `thepost doctor`
предупреждает). Сервис `openbao-init-tls` при первом запуске: `bao operator init` (ключ восстановления и root-токен —
том `openbao-admin`, `init.txt`, только стенд), KV v2 `thepost`, политика `thepost` (`openbao/thepost-policy.hcl`),
AppRole `thepost` (`role_id`, `secret_id` — том `openbao-approle`, владелец 65532), секреты стенда:

| Секрет | Поля | Значение |
|---|---|---|
| `thepost/db` | `username`, `password` | роль `thepost` в PostgreSQL; пароль случайный |
| `thepost/s3` | `access_key_id`, `secret_access_key` | ключи SeaweedFS (`compose/seaweedfs-s3.json`) |

`postgres-role-tls` создаёт в PostgreSQL роль `thepost` (LOGIN, без SUPERUSER, BYPASSRLS, CREATEDB, CREATEROLE) —
владельца БД `thepost`, миграции идут ею же; новый пароль берёт из `openbao-init-tls` через том `pg-bootstrap`
и удаляет файл. Роль `postgres` — администратор стенда и тестов, приложению не выдаётся.
Администрирование OpenBao стенда (root-токен из тома `openbao-admin`):
```bash
docker compose -f deploy/compose/dev.yml run --rm --no-deps --entrypoint sh openbao-init-tls -c \
  'export BAO_TOKEN=$(awk "/Initial Root Token/ {print \$NF}" /admin/init.txt); bao status; bao kv get -mount=thepost s3'
```

### Профиль dev (открытые каналы, интеграционные тесты)
```bash
docker compose -f deploy/compose/dev.yml --profile dev up -d --wait
docker compose -f deploy/compose/dev.yml --profile dev ps   # postgres, seaweedfs, valkey, openbao — healthy
```
Подключение к этим сервисам — открытый канал: в конфигурации `thepost` он допускается только явной настройкой
`allow_plaintext: true` в секции канала с предупреждением в лог при каждом запуске (настройку вводят полосы каналов).
Бакет `thepost-test` создаётся при старте SeaweedFS (`compose/seaweedfs-start.sh`, имя — `TP_S3_BUCKET`).
Остановить с удалением данных: `docker compose -f deploy/compose/dev.yml --profile dev down -v`.

| Сервис | Адрес с хоста | Учётные данные (только dev) |
|---|---|---|
| PostgreSQL 16 | `localhost:5432` | `postgres` / `postgres`, БД `postgres` |
| SeaweedFS S3 | `http://localhost:8333` | ключ `thepost`, секрет `thepost-dev-secret` (`compose/seaweedfs-s3.json`); бакет `thepost-test` |
| Valkey | `localhost:6379` | без пароля |
| Rspamd | `http://localhost:11333` (проверка), `http://localhost:11334` (веб) | пароль веб-интерфейса `thepost-dev` |
| PostgreSQL, роль приложения | `localhost:5432` | `thepost` / `thepost-dev`, БД `thepost` (без SUPERUSER и BYPASSRLS) |
| OpenBao (dev-режим, в памяти) | `http://localhost:8200` | root-токен `thepost-dev-root`; AppRole `thepost`: `role_id` `thepost-dev`, `secret_id` `thepost-dev-secret-id` |

OpenBao dev-режима — те же KV `thepost`, политика, AppRole и секреты стенда (`thepost/db` с паролем `thepost-dev`,
`thepost/s3`); данные в памяти, после перезапуска контейнера их снова записывает `openbao-init`.
Учётные данные профиля `tls` — те же, кроме роли `thepost` и OpenBao (см. выше). Занятый порт хоста переопределяется
переменной: `TP_PG_PORT`, `TP_S3_PORT`, `TP_VALKEY_PORT`, `TP_OPENBAO_PORT`, `TP_RSPAMD_PORT`, `TP_RSPAMD_UI_PORT`
(например, `TP_PG_PORT=55432 docker compose -f deploy/compose/dev.yml --profile dev up -d`).

## Интеграционные тесты
Тест с внешней зависимостью пропускается, если её переменная не задана (`tp_testkit::pg_url()` и аналоги).
Зависимости — профиль `dev`:
```bash
export TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres
export TEST_S3_URL=http://localhost:8333
export AWS_ACCESS_KEY_ID=thepost AWS_SECRET_ACCESS_KEY=thepost-dev-secret AWS_REGION=us-east-1
export TEST_VALKEY_URL=redis://localhost:6379
export TEST_OPENBAO_URL=http://localhost:8200 TEST_OPENBAO_TOKEN=thepost-dev-root
# TEST_LDAP_URL — каталога в compose пока нет; тесты LDAP пропускаются.
```
Прогон (как в конце волны, `docs/DEV.md`):
```bash
cargo nextest run -p <крейты> --no-tests=pass
```
Без переменных выполняются только тесты на in-memory реализациях `tp-testkit`.

## Эталон SMB в контейнерах (профиль smb)
`thepost` (`roles: [all]`, локальный BLOB, шина в процессе) + PostgreSQL (TLS, `sslmode=verify-full`, CA из тома `tls`)
+ OpenBao (TLS, Raft, автоматическое распечатывание) + Rspamd; конфигурация — `compose/thepost-smb.yaml`.
`thepost` подключается к PostgreSQL ролью `thepost` (без SUPERUSER и BYPASSRLS), пароль берёт из OpenBao
(`{ secret: "thepost/db#password" }`), в OpenBao входит по AppRole (том `openbao-approle` → `/etc/thepost/openbao`).
```bash
docker compose -f deploy/compose/dev.yml --profile smb up -d --build
docker compose -f deploy/compose/dev.yml --profile smb run --rm thepost doctor   # без Fail (PLT-12, META-17)
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

Секретов в `config.yaml` нет — только ссылки на OpenBao (секция `secrets`, `database.password`). Вход узла — AppRole:
`role_id` — `/etc/thepost/openbao/role_id`, `secret_id` — `/etc/thepost/credentials/openbao-secret-id` (0600 root),
процессу его передаёт systemd (`LoadCredential=` в `thepost.service`). Оба файла создаёт роль Ansible `openbao`.
С TPM `secret_id` можно зашифровать: `systemd-creds encrypt --with-key=tpm2 --name=openbao-secret-id …`
и заменить `LoadCredential=` на `LoadCredentialEncrypted=` (drop-in `systemctl edit thepost`).

## PostgreSQL: роль приложения
Роль `thepost` — LOGIN, без SUPERUSER, BYPASSRLS, CREATEDB, CREATEROLE, REPLICATION; владелец БД `thepost` (миграции —
той же ролью). С SUPERUSER или BYPASSRLS RLS не изолирует организации, `thepost doctor` — `Fail` (решение #44).
Пароль — только в OpenBao (`thepost/db`, поля `username`, `password`). Создание вручную (администратор СУБД):
```bash
export BAO_ADDR=https://openbao.example.ru:8200 BAO_TOKEN=<токен администратора>
TP_DB_PASSWORD=$(bao kv get -mount=thepost -field=password db) PGHOST=<адрес> PGUSER=postgres \
  sh deploy/compose/postgres/app-role.sh
```

## OpenBao (хранилище секретов)
Эталонная поставка включает OpenBao (MPL-2.0; подходит и существующий HashiCorp Vault заказчика): хранилище Raft,
канал — только TLS, KV v2 `thepost`, политика `thepost` (`openbao/thepost-policy.hcl`: только чтение `thepost/*`
и продление своего токена). Распечатывание после перезапуска — автоматическое (решение владельца 2026-10-30):

| Способ | Когда | Настройка |
|---|---|---|
| TPM или HSM через PKCS#11 | есть TPM 2.0 (`tpm2-pkcs11`) или HSM | Ansible `openbao_seal: pkcs11`, `openbao_seal_options` |
| Облачный KMS, transit другого OpenBao/Vault | облако, Kubernetes | `awskms`, `gcpckms`, `azurekeyvault`, `ocikms`, `transit` |
| Ключ в файле (seal `static`) | нет ни TPM/HSM, ни KMS (по умолчанию) | файл 0400 `openbao`; `thepost doctor` предупреждает |
| Ручной ввод ключей | по требованию безопасности | Ansible `openbao_seal: shamir`; почта ждёт `bao operator unseal` |

### ВМ (Ansible)
```bash
ansible-playbook -i <инвентарь> deploy/ansible/openbao.yml \
  -e openbao_tls_cert_src=server.pem -e openbao_tls_key_src=server.key -e openbao_tls_ca_src=ca.pem
```
Группа инвентаря `openbao`; переменные — `ansible/roles/openbao/defaults/main.yml`. Роль ставит пакет релиза OpenBao
(проверка по `checksums.txt`), пишет `/etc/openbao/openbao.hcl`, при первом прогоне — `bao operator init`
(ключи восстановления и root-токен — `/root/openbao-init.json`, перенести в надёжное место; дальнейшие прогоны —
`-e openbao_admin_token=…`), настраивает KV, политику и AppRole `thepost`, кладёт `role_id` и `secret_id` для
`thepost.service`, создаёт случайный пароль `thepost/db` и отдаёт его фактом `thepost_db_password` роли PostgreSQL.
Повторный прогон ничего не меняет.

### Kubernetes (Helm)
```bash
helm repo add openbao https://openbao.github.io/openbao-helm
helm install openbao openbao/openbao --version 0.30.6 -n openbao --create-namespace -f deploy/helm/openbao/values.yaml
kubectl -n openbao exec -it openbao-0 -- bao operator init   # ключи восстановления и root-токен — администратору
BAO_TOKEN=<root-токен> sh deploy/helm/openbao/bootstrap.sh openbao thepost
```
До установки — секреты `openbao-tls` (сертификат, ключ, CA) и, без KMS/HSM, `openbao-unseal` (`helm/openbao/values.yaml`).
Узел `thepost` входит через Kubernetes auth: сервисный аккаунт `thepost` в пространстве `thepost`, роль `thepost`.
