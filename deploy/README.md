# deploy — окружение разработчика и средства развёртывания

| Путь | Содержание |
|---|---|
| `docker/Dockerfile` | OCI-образ: сборка на `rust:<ver>-bookworm`, запуск на `distroless/cc-debian12:nonroot` |
| `compose/dev.yml` | Зависимости: только TLS (профиль `tls`, по умолчанию) и открытые для тестов (`dev`); эталон SMB (`smb`) |
| `compose/tls/` | Генерация самоподписанного CA (`gen-certs.sh`), `pg_hba.conf` (только `hostssl`) |
| `compose/openbao/` | OpenBao в compose: конфигурация (Raft, TLS, seal `static`), запуск, начальная настройка (`init.sh`) |
| `compose/postgres/app-role.sh` | Учётные записи PostgreSQL: владелец схемы `thepost_owner`, узел `thepost`, только чтение `thepost_readonly` |
| `openbao/thepost-policy.hcl` | Политика узла `thepost` в OpenBao (общая для compose, Helm, Ansible) |
| `helm/openbao/` | Значения чарта `openbao/openbao` и настройка Kubernetes auth (`bootstrap.sh`) |
| `helm/thepost/` | Чарт узла: не root, корневая ФС только для чтения, без capabilities, seccomp `RuntimeDefault`; HPA по CPU, по соединениям — опция (`autoscaling`), PodDisruptionBudget, VPA в режиме рекомендаций, anti-affinity по узлам; PostgreSQL — CloudNativePG (`cnpg.enabled`) |
| `helm/prometheus-adapter/` | Значения чарта prometheus-adapter: метрика `tp_load_connections` для HPA по соединениям |
| `ansible/` | Эталон SMB одной командой (`smb.yml`: роли `thepost_pki`, `openbao`, `postgresql`, `rspamd`, `clamav_milter`, `thepost`); OpenBao отдельно (`openbao.yml`); СУБД Enterprise-ВМ — pg_auto_failover (`pg-ha.yml`); «Малый Enterprise» — 2 узла mailbox и 3 ВМ данных: pg_auto_failover, OpenBao (Raft), SeaweedFS с метаданными filer в PostgreSQL (`small-enterprise.yml`, роль `seaweedfs`) |
| `stand/` | Стенд SMB и базовая нагрузка (APP-D): ВМ SMB + генератор, imaptest, smtp-source, k6, отчёт |
| `systemd/` | Юнит `thepost.service` с hardening, сокеты `thepost-*.socket` (порты ниже 1024), пользователь `thepost` (sysusers) |
| `config/` | Примеры `config.yaml` для пресетов `all` (SMB), `mailbox` (Enterprise-ВМ), `edge` (DMZ) |

## Чистая машина
1. Linux x86_64/AArch64 или Windows + WSL2 Ubuntu (работать внутри WSL, репозиторий — в файловой системе WSL).
2. Docker Engine с Compose v2 (`docker compose version`) или Docker Desktop с интеграцией WSL.
3. Rust через rustup (тулчейн подхватится из `rust-toolchain.toml`), `cargo install cargo-nextest --locked`.
   Пакеты для сборки: `build-essential pkg-config`; с фичей `gssapi` (как выпуск и образ) — ещё `libkrb5-dev libclang-dev`;
   клиенты для проверок (необязательно): `postgresql-client`, `awscli`.
4. `git clone https://github.com/Dicobraz421/ThePost && cd ThePost`.

## Зависимости
Внутренние каналы — только TLS (решение 2026-10-30); открытые каналы — только профиль `dev`, явной настройкой.
Секреты `thepost` — только из OpenBao (решение 2026-10-30): в `config.yaml` — ссылки `{ secret: "<путь>#<поле>" }`.
Профили `tls` и `dev` публикуют одни и те же порты хоста — запускается один из них.

### Профиль tls (по умолчанию)
```bash
docker compose -f deploy/compose/dev.yml up -d --wait   # tls-init, postgres-tls, seaweedfs-tls, openbao-tls
```
Профиль `tls` выбран по умолчанию в `compose/.env`. Сервис `tls-init` при первом запуске создаёт самоподписанный CA
и серверный сертификат (SAN: `localhost`, `127.0.0.1`, имена сервисов) в томе `tls`; копия CA — `compose/tls/ca.pem`.
Новый CA — `down -v`; сменился список SAN — сертификат перевыпускается при `up`. Вне compose:
`sh deploy/compose/tls/gen-certs.sh <каталог>`.

| Сервис | Адрес с хоста | Подключение |
|---|---|---|
| PostgreSQL 16 | `localhost:5432` | только TLS (`pg_hba`: `hostnossl … reject`): `sslmode=verify-full&sslrootcert=deploy/compose/tls/ca.pem` |
| SeaweedFS S3 | `https://localhost:8333` | только https; CA — `compose/tls/ca.pem`; бакет `thepost-test` |
| OpenBao 2.7 | `https://localhost:8200` | только TLS; CA — `compose/tls/ca.pem`; вход узла — AppRole `thepost` |

OpenBao профилей `tls` и `smb` — хранилище Raft (том `openbao-data`), распечатывание после перезапуска —
автоматическое ключом из файла (seal `static`, том `openbao-seal`; на стенде нет TPM/HSM и KMS, `thepost doctor`
предупреждает). Сервис `openbao-init-tls` при первом запуске: `bao operator init` (ключ восстановления и root-токен —
том `openbao-admin`, `init.txt`, только стенд), KV v2 `thepost`, политика `thepost` (`openbao/thepost-policy.hcl`),
AppRole `thepost` (`role_id`, `secret_id` — том `openbao-approle`, владелец 65532), секреты стенда:

| Секрет | Поля | Значение |
|---|---|---|
| `thepost/db` | `username`, `password` | вход узла `thepost` в PostgreSQL; пароль случайный |
| `thepost/db-owner` | `username`, `password` | владелец схемы `thepost_owner` (миграции, `database.owner`) |
| `thepost/db-readonly` | `username`, `password` | только чтение `thepost_readonly`; узлу недоступен (политика `thepost`) |
| `thepost/s3` | `access_key_id`, `secret_access_key` | ключи SeaweedFS (`compose/seaweedfs-s3.json`) |
| `thepost/tls` | `key` | ключ TLS слушателей узла (`tls.key`) — `server.key` тома `tls`; после перевыпуска — новая версия |

`postgres-role-tls` создаёт в PostgreSQL учётные записи (раздел «PostgreSQL: учётные записи» ниже); новые пароли
берёт из `openbao-init-tls` через том `pg-bootstrap` и удаляет файлы. Роль `postgres` — администратор стенда и тестов, приложению не выдаётся.
Администрирование OpenBao стенда (root-токен из тома `openbao-admin`):
```bash
docker compose -f deploy/compose/dev.yml run --rm --no-deps --entrypoint sh openbao-init-tls -c \
  'export BAO_TOKEN=$(awk "/Initial Root Token/ {print \$NF}" /admin/init.txt); bao status; bao kv get -mount=thepost s3'
```

### Профиль dev (открытые каналы, интеграционные тесты)
```bash
docker compose -f deploy/compose/dev.yml --profile dev up -d --wait
docker compose -f deploy/compose/dev.yml --profile dev ps   # postgres, seaweedfs, openbao — healthy
```
Подключение к этим сервисам — открытый канал: в конфигурации `thepost` он допускается только явной настройкой
`allow_plaintext: true` в секции канала с предупреждением в лог при каждом запуске (настройку вводят полосы каналов).
Бакет `thepost-test` создаётся при старте SeaweedFS (`compose/seaweedfs-start.sh`, имя — `TP_S3_BUCKET`).
Остановить с удалением данных: `docker compose -f deploy/compose/dev.yml --profile dev down -v`.

| Сервис | Адрес с хоста | Учётные данные (только dev) |
|---|---|---|
| PostgreSQL 16 | `localhost:5432` | `postgres` / `postgres`, БД `postgres` |
| SeaweedFS S3 | `http://localhost:8333` | ключ `thepost`, секрет `thepost-dev-secret` (`compose/seaweedfs-s3.json`); бакет `thepost-test` |
| Rspamd | `http://localhost:11333` (проверка), `http://localhost:11334` (веб) | пароль веб-интерфейса `thepost-dev` |
| PostgreSQL, учётные записи The Post | `localhost:5432` | узел `thepost` / `thepost-dev`, владелец схемы `thepost_owner` / `thepost-dev-owner`, только чтение `thepost_readonly` / `thepost-dev-readonly`; БД `thepost` |
| OpenBao (dev-режим, в памяти) | `http://localhost:8200` | root-токен `thepost-dev-root`; AppRole `thepost`: `role_id` `thepost-dev`, `secret_id` `thepost-dev-secret-id` |

OpenBao dev-режима — те же KV `thepost`, политика, AppRole и секреты стенда (`thepost/db`, `thepost/db-owner`,
`thepost/db-readonly` с паролями выше, `thepost/s3`); данные в памяти, после перезапуска контейнера их снова записывает `openbao-init`.
Учётные данные профиля `tls` — те же, кроме роли `thepost` и OpenBao (см. выше). Занятый порт хоста переопределяется
переменной: `TP_PG_PORT`, `TP_S3_PORT`, `TP_OPENBAO_PORT`, `TP_RSPAMD_PORT`, `TP_RSPAMD_UI_PORT`
(например, `TP_PG_PORT=55432 docker compose -f deploy/compose/dev.yml --profile dev up -d`).

## Интеграционные тесты
Тест с внешней зависимостью пропускается, если её переменная не задана (`tp_testkit::pg_url()` и аналоги).
Зависимости — профиль `dev`:
```bash
export TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres
export TEST_S3_URL=http://localhost:8333
export AWS_ACCESS_KEY_ID=thepost AWS_SECRET_ACCESS_KEY=thepost-dev-secret AWS_REGION=us-east-1
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
+ OpenBao (TLS, Raft, автоматическое распечатывание) + Rspamd; конфигурация — пресет `config/all.yaml` и значения
установки в переменных `THEPOST_*` (`compose/dev.yml`, служба `thepost`; см. «Одна конфигурация SMB»).
`thepost` подключается к PostgreSQL ролью `thepost` (член `thepost_app`, не владелец схемы), миграции — от владельца
схемы `thepost_owner` (`database.owner`); пароли берёт из OpenBao (`thepost/db`, `thepost/db-owner`), в OpenBao входит
по AppRole (том `openbao-approle` → `/etc/thepost/openbao`). Контейнер — nonroot 65532, `cap_drop: [ALL]`,
`no-new-privileges`, корневая ФС только для чтения; Admin API — `127.0.0.1:8443` внутри контейнера (не публикуется).
```bash
docker compose -f deploy/compose/dev.yml --profile smb up -d --build
docker compose -f deploy/compose/dev.yml --profile smb run --rm thepost doctor   # без Fail (PLT-12, META-17)
```
Порты 25, 465, 587, 143, 993, 4190, 80, 443 публикуются на хост; переопределение — `TP_SMTP_PORT`,
`TP_SUBMISSIONS_PORT`, `TP_SUBMISSION_PORT`, `TP_IMAP_PORT`, `TP_IMAPS_PORT`, `TP_MANAGESIEVE_PORT`, `TP_HTTP_PORT`,
`TP_HTTPS_PORT`.
Отдельная сборка образа: `docker build -f deploy/docker/Dockerfile -t thepost .` (`--build-arg RUST_VERSION=…`).

Слушатели эталона: SMTP 25 (STARTTLS), submission 587 (STARTTLS) и 465 (неявный TLS), IMAP 143 (STARTTLS) и 993,
ManageSieve 4190 (STARTTLS, `sieve.managesieve.listen`), JMAP — HTTPS 443 (`http.jmap`); порт 80 публикуется, но не
слушается. Домен — `example.test`
(принятый домен организации в БД: его заводит `thepost admin init`). Сертификат — `fullchain.pem` тома `tls` от `tls-init`
(SAN: `localhost`, `thepost`, `mail.example.test`), ключ — только секрет `thepost/tls#key` в OpenBao (`tls.key`,
решение 100; записывает `openbao-init-tls`), CA для клиентов — `deploy/compose/tls/ca.pem`.
Проверка эталона (полная — `docs/manual-tests/CMP-01.md`):
```bash
CA=deploy/compose/tls/ca.pem
openssl s_client -connect localhost:465 -CAfile $CA -verify_return_error </dev/null   # 220 mail.example.test, код 0
openssl s_client -connect localhost:587 -starttls smtp -CAfile $CA </dev/null         # то же для 25
openssl s_client -connect localhost:993 -CAfile $CA -verify_return_error </dev/null   # * OK; 143 — -starttls imap
openssl s_client -connect localhost:4190 -starttls sieve -CAfile $CA </dev/null       # "IMPLEMENTATION" … OK
curl --cacert $CA -si https://localhost/.well-known/jmap | head -1                     # HTTP/1.1 401 без учётной записи
```
Письмо между двумя пользователями: учётные записи `a@example.test`, `b@example.test` (способ — issue #193), отправка
через 587 с AUTH, получение через IMAP или JMAP — шаги в `docs/manual-tests/CMP-01.md` (после CMP-04, CMP-05).

## Одна конфигурация SMB (решение 105)
Конфигурация эталона SMB — только пресет `deploy/config/all.yaml`: роли, хранилище, сертификат, слушатели, секреты.
Compose и Ansible его не копируют, а подставляют значения своей установки — секции у них одинаковые:

| Параметр | Пресет | Compose (`THEPOST_*`) | Ansible (инвентарь) |
|---|---|---|---|
| `domain` | `example.ru` | `example.test` | `thepost_domain` |
| `database.url` | `localhost`, CA `/etc/thepost/pki` | `postgres-tls`, CA тома `tls` | `thepost_db_url` (+ `thepost_db_urls`) |
| `database.owner.user` | `thepost_owner` | — | `thepost_db_owner_role` |
| `smtp.hostname` | `mail.example.ru` | `mail.example.test` | `thepost_hostname` |
| `smtp.milter` | — | — | `thepost_rspamd_milter_address`, `thepost_clamav`, `thepost_milter_submission` (адреса фильтров; профиль письма — политика) |
| `http.admin.listen` | `127.0.0.1:8443` | — | `thepost_admin_listen` |
| `secrets.url`, `secrets.tls.ca_file` | `localhost:8200`, `/etc/thepost/pki` | `openbao-tls:8200`, том `tls` | `thepost_secrets_url`, `thepost_internal_ca_file` |
| `secrets.auth.approle` | `role_id` — `/etc/thepost/openbao`, `secret_id` — учётные данные systemd | `secret_id` — том `openbao-approle` | `thepost_secrets_role_id_file` |
| `tls`, `acme` | цепочка `/etc/thepost/tls`, ключ — секрет `thepost/tls#key` | том `tls` | `thepost_tls_mode` (`acme` — секция `acme`, цепочка `/var/lib/thepost/tls`), `thepost_tls_names`, `thepost_acme_email` |
| `blob.gc.window` | по умолчанию | — | `thepost_blob_gc_window` |
| `maintenance` | очистка по сроку, книга GAL, проверка BLOB `verify` (03:00 UTC, раз в сутки) | — | `thepost_maintenance` |
| `cli.oidc` | вход с устройства выключен (`client_id` не задан) | — | `thepost_cli_oidc` |
| `dav`, `http.dav` | лимиты DAV | — | `thepost_dav`, `thepost_dav_listen` |

Остальное (слушатели 25, 465, 587, 143, 993, 443, ManageSieve 4190, роли, BLOB) — из пресета без изменений; новый параметр эталона
добавляется в пресет, а не в compose или шаблон роли. Проверка пресета — `thepost config check --config
deploy/config/all.yaml`.

Лимиты отправки, селектор DKIM и профиль Milter — не конфигурация, а политика организации (цепочка «организация →
домен → учётная запись», решение 97): ни пресет, ни compose, ни Ansible их не задают. Конфигурация хранит только
ключи DKIM (`smtp.dkim.domains`) и адреса фильтров (`smtp.milter.filters`, наборы — `smtp.milter.profiles`). Без
политики действуют умолчания: 1000 писем и 2000 адресатов в час на учётную запись, 100 получателей письма, подпись
всеми ключами домена, все фильтры. Значения эталона — после `thepost admin init` и `thepost admin login`:
```sh
cat > policy.json <<'JSON'
{"send_per_hour": 1000, "rcpts_per_hour": 2000, "max_recipients": 100, "dkim_selector": "s2026"}
JSON
thepost admin policy set --file policy.json        # уровень организации; --domain <домен>, --account <вход>
thepost admin policy get                            # заданное и действующее
```
`policy set` заменяет политику уровня целиком: поля, которых нет в файле, на уровне не заданы. Селектор — из
`smtp.dkim.domains.<домен>.keys`, профиль (`"milter_profile": "<имя>"`) — из `smtp.milter.profiles`.

## Эталон SMB одной командой (Ansible)
Чистая ВМ → PostgreSQL 16 (PGDG), OpenBao, Rspamd (clamav-milter — `thepost_clamav: true`) и `thepost` (все роли) под
systemd (SRS 1.2, 5.6). ОС (решения 2026-11-11 №28, 51, 52; семейство — по менеджеру пакетов):
- apt — Debian 13 (основа) и 12, Ubuntu 26.04 и 24.04, Astra Linux SE 1.8 (наборы PGDG и Rspamd — `bookworm`);
- dnf — AlmaLinux 10 и 9 (совместимость с RHEL 9 и 10): PGDG yum, Rspamd `rspamd.com/rpm-stable`, restic —
  EPEL (на самом RHEL EPEL подключает администратор); РЕД ОС 8 — PostgreSQL, Rspamd, pgBackRest и restic из
  репозиториев дистрибутива, ClamAV в них нет. Включённый firewalld — роль `thepost` открывает порты клиентских
  протоколов (ACME — и 80); SELinux — контексты каталогов pgBackRest (роль `pgbackrest`, `tasks/selinux.yml`).
```bash
cargo build --release                                   # или thepost_binary_url + thepost_binary_checksum
ansible-galaxy collection install -r deploy/ansible/requirements.yml
cp deploy/ansible/inventory.example.yml inventory.yml   # адрес ВМ, домен, способ сертификата
ansible-playbook -i inventory.yml deploy/ansible/smb.yml
```
Порядок: внутренний CA (`thepost_pki`, `/etc/thepost/pki`: внутренние каналы PostgreSQL и OpenBao — только TLS, на
loopback) → OpenBao (`https://localhost:8200`, пароли `thepost/db`, `thepost/db-owner`, `thepost/db-readonly`, AppRole)
→ PostgreSQL (`hostssl` только для учётных записей The Post, без SUPERUSER и BYPASSRLS; `postgres` запускается
юнитом напрямую, без `pg_ctlcluster` на Perl) → Rspamd (Milter `127.0.0.1:11332`) → clamav-milter (`127.0.0.1:7357`) →
`thepost` (sysusers, юнит и сокеты, `config.yaml` — пресет `deploy/config/all.yaml` со значениями инвентаря, с
проверкой `thepost config check`, ожидание
`/healthz/ready`). Порты ниже 1024 — только сокеты systemd (`thepost-*.socket`, решение 102), у процесса нет
capabilities.
Admin API — `thepost_admin_listen` (`127.0.0.1:8443`).
Сертификат клиентских протоколов (решение 101) — `thepost_tls_mode`: `acme` (по умолчанию для публичного имени:
встроенный клиент thepost, секция `acme`, Let's Encrypt HTTP-01 на порту 80 через `thepost-http.socket`; DNS указывает
на ВМ), `internal` (внутренний CA — стенд), `files` (свои PEM). Ключ TLS — только в OpenBao (решение 100): секрет
`thepost/tls#key`, в `config.yaml` (из пресета) — `tls.key: { secret: "thepost/tls#key" }`, на диске ВМ файла ключа
нет (прежние `privkey.pem` роль удаляет); цепочка — файл `tls.cert_file`. Ключ записывает роль (`internal` — выпускает
его в памяти, `files` — с управляющей машины), при `acme` — сам узел (политика `thepost` разрешает ему запись только
этого пути). Новая версия секрета узел подхватывает без перезапуска.
Дополнительные параметры `config.yaml` — `thepost_config_extra` (рекурсивно поверх эталона).
Первая строка `config.yaml` из роли — метка `# thepost: managed-by=ansible` (из чарта `helm/thepost` —
`# thepost: managed-by=helm`, DEP-55): файл правят через инвентарь или значения чарта, текстовый интерфейс открывает
его только на чтение. Для YAML это комментарий, `thepost config check` его не читает.
Системная библиотека MIT Kerberos (`thepost_system_packages`: apt — `libgssapi-krb5-2`, dnf — `krb5-libs`) ставится до
бинарного файла: выпуск и бета собраны с фичей `gssapi`, без неё `thepost` не запускается. Вход по Kerberos —
`thepost_gssapi_keytab_src` (раздел «Вход по Kerberos (GSSAPI)» ниже).
Повторный прогон ничего не меняет. Ключи восстановления и root-токен OpenBao — `/root/openbao-init.json` (см. ниже).

### Резервное копирование и восстановление (роль `pgbackrest`, DEP-15)
`thepost_backup: true` (по умолчанию) — последняя роль `smb.yml`, готовые средства из пакетов дистрибутива и PGDG:
- pgBackRest: `archive_command` (`conf.d/pgbackrest.conf`, `archive_timeout = 60`), stanza `thepost`, хранилище
  `thepost_backup_repo` — `posix` (`/var/lib/pgbackrest`) или `s3` (HTTPS, свой CA — `thepost_backup_s3_ca_file`),
  шифрование `aes-256-cbc`, срок `repo1-retention-full-type=time` (`thepost_backup_retention_days`); таймеры
  `thepost-backup-full.timer` и `thepost-backup-diff.timer` → `thepost-backup@full|diff.service` (от `postgres`);
- каталог BLOB — `thepost-backup-blob.service` после каждой копии СУБД (`Wants=` + `Before=`) и по
  `thepost-backup-blob.timer` (`thepost_backup_blob_schedule`): restic (`backup`, `forget --keep-within --prune`)
  или rsync (зеркало без `--delete`: объекты неизменяемы и удаляются после окна W — надмножество любого момента);
- инвариант W (SRS 3.5): роль `thepost` пишет `blob.gc.window` = глубина PITR + 8 суток (`thepost_blob_gc_window`),
  роль `pgbackrest` проверяет, что W не меньше глубины PITR;
- секреты — KV `thepost-backup` в OpenBao (политика узла `thepost` его не видит): `pgbackrest#cipher_pass`,
  `restic#password` (случайные при первом прогоне), `s3#access_key_id,secret_access_key` (заводит администратор).
  На ВМ — только `systemd-creds encrypt` (`/etc/thepost/credentials/*.cred`; ключ ВМ и TPM2, если есть) и
  `LoadCredentialEncrypted=` юнитов копий и drop-in `postgresql@16-main.service.d/pgbackrest.conf` (для
  `archive_command` и `restore_command`). pgBackRest читает их через `--config-include-path=/run/credentials/<юнит>`,
  restic — через `RESTIC_PASSWORD_FILE` и `AWS_SHARED_CREDENTIALS_FILE`. Вручную — `thepost-pgbackrest …` и
  `thepost-restic …` от root (обёртки `systemd-run` с теми же учётными данными); через `sudo` в семействе RHEL —
  полным путём `/usr/local/sbin/thepost-…`: `secure_path` sudo там без `/usr/local/sbin` (так их вызывают роль и
  `restore.yml`);
- снимок Raft OpenBao (`thepost_backup_openbao: true`, DEP-23; все секреты The Post — DKIM, AppRole, пароли СУБД,
  ключи копий) — `thepost-backup-openbao.service` после каждой копии СУБД (своё расписание —
  `thepost_backup_openbao_schedule`) в то же хранилище: restic (тег `thepost-openbao`) или каталог rsync
  (`thepost_backup_openbao_rsync_dest`). Вход — AppRole `thepost-backup-snapshot` с политикой только на
  `sys/storage/raft/snapshot` (чтение) и `auth/token/revoke-self`; на ВМ — `/etc/thepost/credentials/openbao-snapshot.cred`
  (`systemd-creds`). Снимок зашифрован ключом OpenBao: вне сервера храните `/root/openbao-init.json`,
  `/etc/openbao/unseal.key` (seal `static`) и пароль restic.

Восстановление OpenBao из снимка (новая ВМ или откат секретов) — `restore.yml -e thepost_restore_openbao=true`
с `-e thepost_restore_openbao_init_src=<openbao-init.json прежней ВМ>` и ключом распечатывания прежней ВМ:
`-e thepost_restore_openbao_static_key_src=<unseal.key>` (seal `static`) или `thepost_restore_openbao_unseal_keys`
(Шамир); без ключа — отказ до остановки служб. Снимок (`thepost_restore_openbao_snapshot`, по умолчанию `latest`)
ставится до восстановления СУБД. Новая ВМ: первый `smb.yml -e thepost_backup=false -e thepost_wait_ready=false`
(узел ждёт секретов из снимка — плейбук не ждёт `/healthz/ready`, завершается с предупреждением); копии — по тем же
путям, на перенесённом каталоге копий — `chown -R postgres:postgres /var/lib/pgbackrest` (uid `postgres` на новой ВМ
может быть другим); в новом OpenBao — только `thepost-backup/restic#password` (и `s3`). Ключ TLS `thepost/tls` в
снимке — прежний: после `restore.yml` — повторный `smb.yml` (`internal` перевыпускает сертификат к ключу из снимка,
`files` записывает свой ключ новой версией, `acme` — выпускает новый сам).

Восстановление — `deploy/ansible/restore.yml` (`roles/pgbackrest/tasks/restore.yml`): роль `openbao` (токен
администратора, пароли СУБД) → остановка таймеров и `thepost` → `pgbackrest restore --delta --type=time
--target=<T> --target-action=promote` (без `thepost_restore_target` — `--type=default`) → ожидание конца
восстановления → пароли учётных записей из OpenBao (`accounts.yml`, `always`) → `thepost-restic restore latest`
(или rsync) поверх каталога → `thepost` → `thepost verify --json` (повреждённых и ошибок — 0, иначе отказ) →
`thepost verify --fix` при «сиротах» → таймеры и новая полная копия. Подтверждение — `-e thepost_restore_confirm=true`.
Порядок для администратора и для новой ВМ — [docs/INSTALL.md](../docs/INSTALL.md#резервное-копирование-и-восстановление).

## Установка под systemd (SMB, Enterprise-ВМ)
```bash
sudo apt-get install libgssapi-krb5-2   # РЕД ОС, AlmaLinux, RHEL: sudo dnf install krb5-libs (фича gssapi выпуска)
sudo install -m 0755 target/release/thepost /usr/local/bin/thepost
sudo install -m 0644 deploy/systemd/thepost.sysusers.conf /usr/lib/sysusers.d/thepost.conf && sudo systemd-sysusers
sudo install -D -m 0640 -o root -g thepost deploy/config/all.yaml /etc/thepost/config.yaml   # domain, smtp.hostname, адреса
sudo install -m 0644 deploy/systemd/thepost.service deploy/systemd/thepost-*.socket /etc/systemd/system/
sudo systemd-analyze verify /etc/systemd/system/thepost.service /etc/systemd/system/thepost-*.socket
# Порты клиентских протоколов — только сокеты systemd (у процесса нет capabilities, решение 102):
sudo systemctl daemon-reload && sudo systemctl enable --now thepost-{smtp,submissions,submission,imap,imaps,https,managesieve}.socket
# systemd < 254 (`systemctl --version`; Debian 12, AlmaLinux 9, РЕД ОС 8) — постоянная пауза перезапуска 30 с:
#   sudo install -D -m 0644 deploy/systemd/thepost.service.d/restart-legacy.conf \
#     /etc/systemd/system/thepost.service.d/restart-legacy.conf && sudo systemctl daemon-reload
sudo systemctl enable --now thepost
systemd-analyze security thepost.service   # не хуже 2.0 (ожидается 1.2)
```
Сокеты (`FileDescriptorName=`): `smtp` 25, `submissions` 465, `submission` 587, `imap` 143, `imaps` 993, `https` 443,
`managesieve` 4190; `http` 80 — только проверки встроенного ACME (`acme.enabled`; `systemctl enable --now
thepost-http.socket`). Встроенный ACME вручную — каталоги `sudo install -d -o thepost -g thepost -m 0750
/var/lib/thepost/{tls,acme}` и в `config.yaml`:
```yaml
acme:
  enabled: true
  domains: [mail.example.ru]
  contact: ["mailto:admin@example.ru"]
  state_dir: /var/lib/thepost/acme
tls:
  cert_file: /var/lib/thepost/tls/fullchain.pem   # цепочку пишет узел; ключ — новой версией секрета tls.key
  key: { secret: "thepost/tls#key" }
```
Первый выпуск — до запуска протоколов: если порт 80 недоступен снаружи или DNS имени не указывает на ВМ, узел не
стартует (`acme: сертификат … не выпущен`).
Узел берёт сокет systemd по адресу слушателя (`0.0.0.0:<порт>` или `[::]:<порт>` совпадает с `ListenStream=<порт>`),
иначе открывает свой (порт ниже 1024 без сокета не откроется — capabilities у процесса нет); перезапуск `thepost` не
закрывает сокеты: соединения ждут в очереди.
Hardening юнита: пустой `CapabilityBoundingSet=`, `NoNewPrivileges`, `PrivateUsers`, `ProtectSystem=strict`,
`ProtectHome`, `PrivateTmp`, `PrivateDevices`, `MemoryDenyWriteExecute`, `RestrictAddressFamilies`,
`SystemCallFilter=@system-service` без привилегированных вызовов (свои профили seccomp и Landlock процесс
накладывает сам — секция `confine`).
Перезапуск после сбоя (решение 85): без предела числа попыток (`StartLimitIntervalSec=0`), пауза растёт с 10 с до
5 мин (`RestartSteps=6`, `RestartMaxDelaySec=5min`, systemd 254+; на старых — постоянные 30 с, `restart-legacy.conf`).
После длинной серии сбоев узел вернётся не позже чем через 5 минут после устранения причины; сразу —
`systemctl restart thepost`.

Данные — `/var/lib/thepost` (создаёт systemd). BLOB-каталог вне него — `systemctl edit thepost`:
`[Service]` + `ReadWritePaths=/srv/thepost-blob`. Проверка окружения — `thepost doctor` (SRS 5.5); привилегии
службы (`privileges.*`) — только в её окружении: `docs/manual-tests/DEP-08.md`.

Секретов в `config.yaml` нет — только ссылки на OpenBao (секция `secrets`, `database.password`, ключ TLS `tls.key` —
секрет `thepost/tls#key`: `bao kv put -mount=thepost tls key=@privkey.pem`, файл ключа на ВМ не нужен). Вход узла — AppRole:
`role_id` — `/etc/thepost/openbao/role_id`, `secret_id` — `/etc/thepost/credentials/openbao-secret-id` (0600 root),
процессу его передаёт systemd (`LoadCredential=` в `thepost.service`). Оба файла создаёт роль Ansible `openbao`.
С TPM `secret_id` можно зашифровать: `systemd-creds encrypt --with-key=tpm2 --name=openbao-secret-id …`
и заменить `LoadCredential=` на `LoadCredentialEncrypted=` (drop-in `systemctl edit thepost`).

### Вход по Kerberos (GSSAPI)
SASL GSSAPI (SMTP, IMAP, POP3, ManageSieve; секция `auth.gssapi`) — системная MIT Kerberos (`libgssapi_krb5`: пакет
выше, в образе — своя копия). Keytab узла — по принципалу на службу, `<узел>` — имя, по которому подключаются клиенты
(`gssapi.hostname`): `smtp/<узел>`, `imap/<узел>`, `pop/<узел>` (POP3, RFC 5034), `sieve/<узел>` (ManageSieve,
RFC 5804). Билет одной службы для другой не принимается. `HTTP/<узел>` не нужен: JMAP, DAV и Admin API по Kerberos
не входят.
```bash
# MIT Kerberos (kadmin), область EXAMPLE.RU:
kadmin -q 'addprinc -randkey smtp/mail.example.ru' -q 'addprinc -randkey imap/mail.example.ru' \
       -q 'addprinc -randkey pop/mail.example.ru' -q 'addprinc -randkey sieve/mail.example.ru'
kadmin -q 'ktadd -k thepost.keytab smtp/mail.example.ru imap/mail.example.ru pop/mail.example.ru sieve/mail.example.ru'
# Active Directory: учётная запись узла с SPN smtp/, imap/, pop/, sieve/mail.example.ru (setspn -S), keytab — ktpass или msktutil.
klist -k thepost.keytab   # четыре принципала
```
Keytab MIT Kerberos читает только из `KRB5_KTNAME`; `auth.gssapi.keytab` узел сверяет с этой переменной.
- ВМ (Ansible): `thepost_gssapi_keytab_src` — файл на управляющей машине → `/etc/thepost/thepost.keytab` (0640
  root:thepost), drop-in `thepost.service.d/gssapi.conf` (`Environment=KRB5_KTNAME=FILE:…` и
  `Environment=KRB5RCACHEDIR=/var/lib/thepost/krb5rcache`), `auth.gssapi.keytab`;
  `hostname` и `realms` — `thepost_gssapi`. Без `thepost_gssapi_keytab_src` роль убирает keytab и drop-in.
- Вручную: `sudo install -m 0640 -o root -g thepost thepost.keytab /etc/thepost/thepost.keytab`, `systemctl edit thepost`:
  `[Service]` + `Environment=KRB5_KTNAME=FILE:/etc/thepost/thepost.keytab` и
  `Environment=KRB5RCACHEDIR=/var/lib/thepost/krb5rcache`, в `config.yaml` —
  `auth: { gssapi: { keytab: /etc/thepost/thepost.keytab, hostname: mail.example.ru } }`.
- Kubernetes (Helm): секрет с ключом `keytab` → `gssapi.keytabSecret` (раздел «Узел thepost (Helm)»).

## PostgreSQL: учётные записи
Раздельные учётные записи (SRS 2.7, 3.1; META-23, DEP-08), все — LOGIN без SUPERUSER, BYPASSRLS, CREATEDB, CREATEROLE,
REPLICATION (с SUPERUSER или BYPASSRLS RLS не изолирует организации, `thepost doctor` — `Fail`, решение #44):

| Учётная запись | Права | В config.yaml | Секрет OpenBao |
|---|---|---|---|
| `thepost_owner` | владелец БД `thepost` и схемы `public`; только миграции | `database.owner` | `thepost/db-owner` |
| `thepost` | член `thepost_app`: DML в таблицах ядра, журналы — только добавление | `database.url` | `thepost/db` |
| `thepost_readonly` | член `thepost_reader`: чтение метаданных (отчёты, проверки, роли search/indexer) | — | `thepost/db-readonly` |

Групповые роли `thepost_app`, `thepost_reader` (без входа) создаёт администратор СУБД (скрипт ниже, роль Ansible
`postgresql`), права на таблицы им выдаёт миграция (`tp_grant_roles`). Установка до DEP-08 (таблицы принадлежат
`thepost`) переводится тем же скриптом: владелец — `thepost_owner`. Создание вручную (администратор СУБД):
```bash
export BAO_ADDR=https://openbao.example.ru:8200 BAO_TOKEN=<токен администратора>
TP_DB_PASSWORD=$(bao kv get -mount=thepost -field=password db) \
TP_DB_OWNER_PASSWORD=$(bao kv get -mount=thepost -field=password db-owner) \
TP_DB_READONLY_PASSWORD=$(bao kv get -mount=thepost -field=password db-readonly) \
PGHOST=<адрес> PGUSER=postgres sh deploy/compose/postgres/app-role.sh
```

## PostgreSQL: высокая доступность (Enterprise)
Продукт кластером СУБД не управляет: подходит любой механизм заказчика с синхронной репликой (RPO = 0) и
автоматическим переключением без второго primary (RTO ≤ 30 с), SRS 3.1, решение 2026-11-01. Узел находит primary
сам — список адресов `database.url` + `database.urls` (первый доступный для записи), единый адрес (VIP, DNS) или
Service Kubernetes; при переключении сбрасывает пул и повторяет идемпотентные операции (`database.failover`:
`retry_initial`, `retry_max`, `retry_for`, `probe_timeout`), дольше `retry_for` — временный отказ (SMTP 4xx).
Метрики — `tp_db_failover_*` в `/metrics`. Эталонные стенды — без интерпретаторов в работе (SRS 7.1):

| Профиль | Эталон | Адрес primary в config.yaml |
|---|---|---|
| Enterprise-ВМ | pg_auto_failover: монитор + 3 узла данных (`ansible/pg-ha.yml`) | `url` + `urls` — узлы данных (`config/mailbox.yaml`) |
| Enterprise-K8s | CloudNativePG: Cluster (3 экземпляра) + Pooler (`helm/thepost`, `cnpg.enabled`) | служба `<выпуск>-db-rw`, PgBouncer — `<выпуск>-db-pooler-rw` |

### Enterprise-ВМ (Ansible, pg_auto_failover)
```bash
ansible-galaxy collection install -r deploy/ansible/requirements.yml
cp deploy/ansible/inventory-ha.example.yml inventory-ha.yml   # монитор, узлы, сети, файлы TLS; пароли — ansible-vault
ansible-playbook -i inventory-ha.yml deploy/ansible/pg-ha.yml --ask-vault-pass
```
Роль `pg_auto_failover`: PostgreSQL 16 и pg_auto_failover из PGDG, кластер Debian `main` не создаётся; `pg_autoctl`
и `postgres` — бинарные файлы, юнит `pgautofailover.service` (без `pg_wrapper`/`pg_ctlcluster` на Perl). Узлы данных
ставятся по одному: первый — primary, следующие — реплики; при `db_durability: required` — `number_sync_standbys: 1`
(фиксация ждёт одну из двух реплик), при `preferred` — `0` (см. «Режим сохранности»). Сроки переключения — `pgaf_monitor_settings`, `pgaf_network_partition_timeout` (RTO ≈ 10 с + 15 с; значения
pg_auto_failover по умолчанию дают ≈ 50 с). Только TLS (`verify-full`, `pg_hba` — `hostssl`, открытый канал
отклоняется), служебные пароли — `~postgres/.pgpass`. Учётные записи и БД The Post — на primary, как в эталоне SMB.
Узел `thepost` (роль `thepost`): `thepost_db_url` — первый узел, `thepost_db_urls` — остальные,
`thepost_db_failover` — `database.failover`. Шард 2 — узел своей формации (`pgaf_formation`).
Без PgBouncer: с `pgbouncer: true` слушатель сигналов и миграции идут на `direct_url` — один адрес, `urls` к нему не
применяется. Миграции (`database.owner`) подключаются только к `url`.

### Enterprise-K8s (Helm, CloudNativePG)
```bash
helm repo add cnpg https://cloudnative-pg.github.io/charts
helm install cnpg cnpg/cloudnative-pg -n cnpg-system --create-namespace          # оператор 1.26+
kubectl -n thepost create secret generic thepost-db-owner --type=kubernetes.io/basic-auth \
  --from-literal=username=thepost_owner --from-literal=password="$(bao kv get -mount=thepost -field=password db-owner)"
# так же thepost-db (thepost, thepost/db) и thepost-db-readonly (thepost_readonly, thepost/db-readonly)
helm install thepost deploy/helm/thepost -n thepost --set cnpg.enabled=true -f <свои значения>
```
Cluster: 3 экземпляра на разных узлах Kubernetes, синхронная репликация `ANY 1` с `dataDurability` из
`cnpg.db_durability` (по умолчанию `required`, см. «Режим сохранности»),
без суперпользователя, `pg_hba` — только `hostssl`; образ `minimal` (без barman-cloud на Python). Pooler —
PgBouncer в режиме transaction; чарт сам задаёт `database.url` (Pooler), `pgbouncer: true` и `direct_url` (служба
primary) и монтирует CA сервера (`<выпуск>-db-ca`, только `ca.crt`).

### Режим сохранности: `db_durability` (решение владельца #272)
Что делать, если primary остался без синхронной реплики (обе реплики отказали или отрезаны): ждать реплику или писать
без неё. Переменная — Ansible `db_durability` (роль `pg_auto_failover`, по формации — значение на её узлах данных) и
Helm `cnpg.db_durability`; по умолчанию `required` на ВМ и в K8s. Пока жива хотя бы одна реплика, оба режима
одинаковы: фиксация ждёт её подтверждения (RPO = 0), отказ одной из двух реплик запись не останавливает.

| Режим | Без синхронной реплики | RPO | Доступность записи | pg_auto_failover / CloudNativePG |
|---|---|---|---|---|
| `required` (по умолчанию) | фиксация ждёт возврата реплики: запись (SMTP, IMAP APPEND, JMAP) — временные отказы, чтение работает | 0 всегда | теряется до возврата реплики | `number_sync_standbys` = `pgaf_number_sync_standbys` (≥ 1, кворум — не меньше значения + 1 реплик), primary не переходит в `wait_primary` / `dataDurability: required` |
| `preferred` | запись асинхронная, продолжается | 0, пока реплика есть; отказ primary без реплик теряет последние фиксации (письма, принятые с 250) | сохраняется | `number_sync_standbys: 0`: primary переходит в `wait_primary`, `synchronous_standby_names` пуст / `dataDurability: preferred` |

`required` — RPO = 0 по SRS 3.1 ценой остановки записи при двойном отказе; `preferred` — для установок, где простой
записи хуже потери последних секунд (и для формации из одного узла: роль требует `preferred`, ждать некого).
Смена режима — повторный прогон `pg-ha.yml` (меняет `number_sync_standbys` формации на мониторе; primary применяет
через `apply_settings`, без перезапуска) или `helm upgrade` (оператор меняет `synchronous_standby_names`). Проверка обоих режимов — `docs/manual-tests/DEP-09.md`.

## OpenBao (хранилище секретов)
Эталонная поставка включает OpenBao (MPL-2.0; подходит и существующий HashiCorp Vault заказчика): хранилище Raft,
канал — только TLS, KV v2 `thepost`, политика `thepost` (`openbao/thepost-policy.hcl`: чтение `thepost/*`, запись —
только ключа TLS `thepost/tls` для встроенного ACME, продление своего токена). Распечатывание после перезапуска — автоматическое (решение владельца 2026-10-30):

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
`thepost.service`, создаёт случайные пароли `thepost/db`, `thepost/db-owner`, `thepost/db-readonly` и отдаёт их
фактами `thepost_db_password`, `thepost_db_owner_password`, `thepost_db_readonly_password` роли PostgreSQL.
Повторный прогон ничего не меняет.

### Kubernetes (Helm)
```bash
helm repo add openbao https://openbao.github.io/openbao-helm
helm install openbao openbao/openbao --version 0.30.6 -n openbao --create-namespace -f deploy/helm/openbao/values.yaml
kubectl -n openbao exec -it openbao-0 -- bao operator init   # ключи восстановления и root-токен — администратору
BAO_TOKEN=<root-токен> TLS_KEY=tls.key sh deploy/helm/openbao/bootstrap.sh openbao thepost   # ключ — thepost/tls#key
```
До установки — секреты `openbao-tls` (сертификат, ключ, CA) и, без KMS/HSM, `openbao-unseal` (`helm/openbao/values.yaml`).
Узел `thepost` входит через Kubernetes auth: сервисный аккаунт `thepost` в пространстве `thepost`, роль `thepost`.

### Узел thepost (Helm)
```bash
kubectl -n thepost create secret docker-registry ghcr --docker-server=ghcr.io   --docker-username=<логин GitHub> --docker-password=<токен read:packages>
helm install thepost deploy/helm/thepost -n thepost --set 'imagePullSecrets[0].name=ghcr' -f <свои значения>
```
Образ в ghcr.io закрытый: без `imagePullSecrets` чарт не рендерится (ошибка с подсказкой). Тег по умолчанию —
`appVersion` чарта (версия выпуска, `beta.yml` публикует его вместе с `beta-<дата>`).
Под — `runAsNonRoot` (65532), `readOnlyRootFilesystem`, `allowPrivilegeEscalation: false`, `capabilities.drop: [ALL]`,
seccomp `RuntimeDefault` (свой профиль узла — `containerSecurityContext.seccompProfile` типа `Localhost`) и профили ролей
процесса (секция `confine`). Слушатели — на портах выше 1024, стандартные порты (25, 465, 587, 143, 993, 443) — у Service;
ManageSieve — 4190 и в поде, и у Service.
Admin API — `127.0.0.1:8443` в поде (`kubectl port-forward`), служебный HTTP — адрес пода для проб kubelet.
До установки — секрет `thepost-tls` (`tls.crt`, `ca.crt`; в под монтируются только они) и OpenBao (`bootstrap.sh` выше,
ключ TLS — `TLS_KEY`: секрет `thepost/tls#key`, в values — `config.tls.key`; смена ключа — повторный `bootstrap.sh`).
Вход по Kerberos: `kubectl -n thepost create secret generic thepost-keytab --from-file=keytab=thepost.keytab` и
`--set gssapi.keytabSecret=thepost-keytab` — чарт монтирует keytab в `/etc/thepost/krb5/thepost.keytab`, задаёт
`KRB5_KTNAME`, `KRB5RCACHEDIR` и `config.auth.gssapi.keytab`; `hostname` и `realms` — в `config.auth.gssapi`.
