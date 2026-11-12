# Требования к окружению The Post

Для администратора: что должно быть верно в окружении до установки и как это проверить. Каждая строка —
требование, проверка (команда или строка `thepost doctor`), что будет при нарушении, источник. Как ставить — в
[`INSTALL.md`](INSTALL.md) и [`deploy/README.md`](../deploy/README.md).

**Главная проверка** — `thepost doctor` на узле (`--json` — для автоматизации): итог каждой проверки `OK`, `WARN`,
`FAIL` или `SKIP`, код возврата ≠ 0 при любом `FAIL`. Пороги — секции `doctor` и `meta.doctor` в `config.yaml`.
Проверки привилегий (`privileges.*`) показывают правду только в окружении службы — см. [учётную запись службы](#учётная-запись-службы-и-права).

Источники: [S8](spec/S8.md) — системные требования, [APP-Z](spec/APP-Z.md) — схемы развёртывания, проверки —
[`doctor.rs`](../crates/thepost/src/doctor.rs), [`doctor_dns.rs`](../crates/thepost/src/doctor_dns.rs),
[`tp-meta/src/doctor.rs`](../crates/tp-meta/src/doctor.rs), [`tp-platform/src/privileges.rs`](../crates/tp-platform/src/privileges.rs),
[`tp-platform/src/secrets.rs`](../crates/tp-platform/src/secrets.rs).

## ОС и ядро

| Требование | Проверка | При нарушении | Источник |
|---|---|---|---|
| Linux x86_64 или AArch64 | `uname -m` | бинарного файла под другую платформу нет | [README](../README.md#установка) |
| glibc 2.18 и новее (выпуск собирается `cross` в образе с glibc 2.23; по версиям символов `beta-20261111.3` x86_64 и AArch64 — не выше `GLIBC_2.18`) | `ldd --version` | бинарный файл не запустится: `version 'GLIBC_2.xx' not found` | [`beta.yml`](../.github/workflows/beta.yml), [решение 51](decisions/2026-11-11-stage2-followups.md) |
| Эталон Ansible: apt — Debian 13 (основа) и 12, Ubuntu 26.04 и 24.04, Astra Linux SE 1.8; dnf — РЕД ОС 8, AlmaLinux 10 и 9 (совместимость с RHEL 9 и 10, сам RHEL не проверяется) | `cat /etc/os-release` | плейбук `smb.yml` останавливается на проверке платформы (менеджер пакетов не apt и не dnf) | [`smb.yml`](../deploy/ansible/smb.yml), [INSTALL](INSTALL.md#рабочий-сервер-ansible), решения 28, 51, 52, [DEP-24](manual-tests/DEP-24.md), [DEP-19](manual-tests/DEP-19.md), [DEP-20](manual-tests/DEP-20.md) |
| systemd с `systemd-sysusers` и `LoadCredential=` (юнит, sysusers, учётные данные) | `systemd-analyze verify /etc/systemd/system/thepost.service` | юнит не загрузится; без `LoadCredential=` узел не получит `secret_id` OpenBao | [`thepost.service`](../deploy/systemd/thepost.service) |
| Ядро с seccomp | `privileges.seccomp`: `OK enforce` | `FAIL`: запуск с `confine.seccomp` не выполнится | [`privileges.rs`](../crates/tp-platform/src/privileges.rs) |
| Секция `confine` в `config.yaml` задана явно (`seccomp: enforce`) | `privileges.seccomp` | `WARN log`: без секции вызовы вне профиля только пишутся в журнал, не блокируются | [`doctor.rs`](../crates/thepost/src/doctor.rs) |
| Landlock ABI ≥ 5 (ядро 6.10+) | `privileges.landlock`: `OK abi=<N>` | `WARN abi=<N>` — ограничения ФС ролей частичные (решение 7); `WARN unsupported` — Landlock выключен (`lsm=`) или запрещён seccomp контейнера: доступ к ФС не ограничен | [`privileges.rs`](../crates/tp-platform/src/privileges.rs), [решение 7](decisions/2026-11-11-stage2-followups.md), [DEP-08](manual-tests/DEP-08.md) |
| cgroup v2 с делегированием — только если задан `search.extract.sandbox.cgroup_dir` (drop-in с `Delegate=yes`) | `stat -fc %T /sys/fs/cgroup` → `cgroup2fs` | пул извлечения текста не получит лимитов `memory.max`, `cpu.max`; без `cgroup_dir` cgroup не используется | [`thepost.service`](../deploy/systemd/thepost.service), [`sandbox/pool.rs`](../crates/tp-sys/src/sandbox/pool.rs) |
| Kubernetes: seccomp `RuntimeDefault`, не root, корневая ФС только для чтения, без capabilities | `kubectl get pod … -o yaml` | чарт `helm/thepost` задаёт это сам; ослабление — нарушение SRS 2.7 | [deploy/README](../deploy/README.md#узел-thepost-helm) |

## Железо по профилям

| Профиль | Ресурсы | Источник |
|---|---|---|
| SMB, до 1 000 ящиков | 1 ВМ: 4 vCPU, 8 GB RAM, NVMe — объём почты × 1,3; clamav-milter — ещё 2 GB RAM или отдельный узел | [S8](spec/S8.md), [APP-Z З.1](spec/APP-Z.md) |
| Попробовать (Docker Compose) | любая машина с Docker Engine и Compose v2, от 4 GB RAM | [INSTALL](INSTALL.md#попробовать-docker-compose) |
| Малый Enterprise | 2 узла mailbox по 4 vCPU / 8 GB; PostgreSQL primary и standby по 4 vCPU / 16 GB / NVMe; 3 ВМ данных (монитор, SeaweedFS, OpenBao) | [APP-Z З.2](spec/APP-Z.md) |
| Enterprise-ВМ, Enterprise-K8s | по результатам испытаний на стенде (приложение Д) | [S8](spec/S8.md), [APP-Z З.3, З.4](spec/APP-Z.md) |

Общее для СУБД: рабочий набор (горячие ящики, индексы) — в RAM, хранилище — SSD/NVMe ([S3 п. 3.1](spec/S3.md)).
Проверки размера ВМ в `doctor` нет: при нехватке — рост задержек, проверить — метрики `/metrics`.

## Сеть и порты

Входящие (снаружи):

| Порт | Назначение | Проверка | При нарушении | Источник |
|---|---|---|---|---|
| 25 | приём почты (STARTTLS) | `openssl s_client -connect <сервер>:25 -starttls smtp` | внешняя почта не приходит | [INSTALL](INSTALL.md#что-понадобится) |
| 465, 587 | отправка с входом (TLS, STARTTLS) | `openssl s_client -connect <сервер>:465` | клиенты не отправляют | [INSTALL](INSTALL.md#настройка-клиентов) |
| 993, 143 | IMAP (TLS, STARTTLS) | `openssl s_client -connect <сервер>:993` | клиенты не читают почту | [INSTALL](INSTALL.md#настройка-клиентов) |
| 443 | JMAP, автонастройка | `curl -si https://<сервер>/.well-known/jmap` → `401` | JMAP-клиенты не подключаются | [deploy/README](../deploy/README.md#эталон-smb-в-контейнерах-профиль-smb) |
| 80 | только выпуск сертификата Let's Encrypt (`certbot`, HTTP-01) | `curl -I http://<сервер>/` во время выпуска | сертификат не выпускается | [INSTALL](INSTALL.md#рабочий-сервер-ansible) |

Порты ниже 1024 узел слушает с `CAP_NET_BIND_SERVICE` (`bind-capability.conf`, по умолчанию) или сокетами systemd —
второй вариант пока не работает (TODO(PLT-16)). Проверка: `privileges.capabilities` — `OK`; лишние capabilities — `WARN`.

Исходящие:

| Куда | Зачем | Проверка | При нарушении | Источник |
|---|---|---|---|---|
| TCP 25 к MX получателей (`smtp.queue.port`) или к ретранслятору (`smtp.queue.relay`) | отправка почты наружу | `nc -vz <MX получателя> 25` с сервера | письма копятся в очереди и через `smtp.queue.expire` (5 сут.) возвращаются отправителю | [`tp-smtp/src/config.rs`](../crates/tp-smtp/src/config.rs) |
| HTTPS к доменам получателей | политики MTA-STS | — | обязательный TLS по MTA-STS не применяется | [S4](spec/S4.md) |
| Установка: GitHub Releases, `apt.postgresql.org`, `rspamd.com`, выпуски OpenBao; ACME — Let's Encrypt; ClamAV — зеркала баз freshclam | пакеты, сертификат, антивирусные базы | прогон плейбука | плейбук останавливается; clamd без баз не запускается | [роли Ansible](../deploy/ansible/roles) |

Внутренние (на SMB — только loopback): PostgreSQL `5432`, OpenBao `8200`/`8201`, Rspamd `11332` (Milter), `11333`,
`11334`, clamav-milter `7357`, Admin API `127.0.0.1:8443`, служебный HTTP `127.0.0.1:8081` (`/healthz`, `/metrics`).
Проверка: `ss -ltnp` — Admin API только на `127.0.0.1` ([DEP-08](manual-tests/DEP-08.md)); наружу — нарушение SRS 6.8.

За балансировщиком: PROXY v2 у слушателей (`proxy_protocol: true`) и адреса балансировщиков в `trusted_proxies`;
таймауты балансировщика не меньше времени IDLE ([APP-Z З.3](spec/APP-Z.md)). Проверка — `proxy.v2`: без
`trusted_proxies` — `WARN` (прямые подключения отклоняются).

## DNS и PTR

Проверки по публичному DNS через системный резолвер (`/etc/resolv.conf`); для зон `.test`, `.example`, `.invalid`,
`.localhost` — `SKIP`.

| Требование | Проверка | При нарушении | Источник |
|---|---|---|---|
| Резолвер работает | `dns.resolver` | `FAIL`: DNS-проверки и доставка наружу невозможны | [`doctor_dns.rs`](../crates/thepost/src/doctor_dns.rs) |
| `MX` домена → `smtp.hostname` | `dns.mx` | нет MX или null MX — `FAIL`: внешняя почта не доставляется; MX на другой узел — `WARN` | там же, [INSTALL](INSTALL.md#dns-домена) |
| `TXT` SPF, без `+all` | `dns.spf` | нет — `WARN`, DMARC не пройдёт по SPF; `+all` — `WARN` | там же |
| DKIM: `smtp.dkim.domains` и `TXT <селектор>._domainkey` | `dns.dkim.<домен>` | подпись не настроена — `WARN`; ключ не опубликован — `FAIL`: подписанные письма не пройдут проверку | там же |
| `TXT _dmarc` | `dns.dmarc` | нет — `WARN`: подделку адреса не отклонят | там же |
| MTA-STS | `dns.mta_sts` | нет — `WARN` | там же |
| `smtp.hostname` задан, у него есть `A`/`AAAA` | `dns.ptr` | нет имени — `WARN`: приветствие и PTR не совпадут | там же |
| PTR исходящего адреса (`smtp.queue.source_ip` или адрес `smtp.hostname`) = `smtp.hostname` | `dns.ptr`, `dig -x <адрес>` | нет PTR или другое имя — `WARN`: получатели отклоняют или считают спамом | там же |
| Enterprise: адреса egress — в SPF и PTR | `dns.spf`, `dns.ptr` | письма с этих адресов не пройдут SPF | [APP-Z З.3](spec/APP-Z.md) |
| DANE: системные DNS-серверы отдают подписи DNSSEC | `dig +dnssec <MX> TLSA` | зона считается неподписанной, DANE не применяется | [`tp-smtp/src/policy.rs`](../crates/tp-smtp/src/policy.rs) |

PTR задаёт владелец адреса (провайдер, хостинг), а не DNS домена.

## Время

| Требование | Проверка | При нарушении | Источник |
|---|---|---|---|
| Часы узлов и сервера СУБД синхронизированы (NTP/chrony). Эталоны Ansible (`smb.yml`, `pg-ha.yml`) ставят chrony первой ролью; серверы — `thepost_ntp_servers` (пусто — пул дистрибутива) | `time.clock_skew`: < 500 мс — `OK` (`doctor.warn_clock_skew`), ≥ 5 с — `FAIL` (`doctor.max_clock_skew`); `chronyc tracking` (`Leap status : Normal`), `timedatectl` | лизы, сроки записей, DKIM, TOTP работают неверно | [`doctor.rs`](../crates/thepost/src/doctor.rs), [`sections.rs`](../crates/thepost/src/sections.rs), [роль `chrony`](../deploy/ansible/roles/chrony/defaults/main.yml), [решение 25](decisions/2026-11-11-stage2-followups.md) |
| Часовой пояс сеанса СУБД — UTC (эталоны Ansible задают `timezone = 'UTC'`) | `database.timezone`; `psql -Atc 'SHOW timezone'` → `UTC` | `FAIL` | [`tp-meta/src/doctor.rs`](../crates/tp-meta/src/doctor.rs), [решение 24](decisions/2026-11-11-stage2-followups.md) |
| Журналы СУБД — в UTC (`log_timezone`; эталоны Ansible задают) | `database.log_timezone` | `WARN`: время в журналах СУБД расходится с журналами The Post | там же |

Служба менять часы не может (`ProtectClock=yes`, без `@clock`): синхронизацию ведёт система.

## TLS и сертификаты

| Требование | Проверка | При нарушении | Источник |
|---|---|---|---|
| Сертификат клиентских протоколов (секция `tls`) читается и действителен | `tls.certificate` | истёк — `FAIL`; < 21 сут. (`doctor.cert_warn_before`) — `WARN`; файла нет — `WARN` (при `acme` — `SKIP` до выпуска) | [`doctor_dns.rs`](../crates/thepost/src/doctor_dns.rs) |
| Сертификат покрывает `smtp.hostname` | `tls.names` | `WARN`: клиенты не проверят TLS | там же |
| Способ выпуска — `thepost_tls_mode`: `certbot` (порт 80, DNS на ВМ), `acme`, `files`, `internal` (только стенд) | прогон плейбука | плейбук отклоняет неверный режим или пустой `thepost_acme_email` | [роль `thepost`](../deploy/ansible/roles/thepost/defaults/main.yml) |
| Клиенты — TLS 1.3; TLS 1.2 только настройкой `tls.min_version: "1.2"` | `openssl s_client -tls1_2 -connect <сервер>:993` | старые клиенты без TLS 1.3 не подключатся | [S6 п. 6.2](spec/S6.md), [`tls.rs`](../crates/tp-platform/src/tls.rs) |
| Внутренние каналы (PostgreSQL, S3, OpenBao, служебный HTTP) — только TLS с проверкой имени | `database.tls`, `secrets.config` | открытый канал — только профиль `dev` (`allow_plaintext`) с `WARN` | [S6 п. 6.2](spec/S6.md), [решение 2026-10-30](decisions/2026-10-30-owner-answers.md) |
| Эталон SMB: внутренний CA `thepost_pki` (CA — 10 лет, сертификат служб — 825 сут.). Прогон плейбука перевыпускает сертификат тем же CA, если до конца срока < 90 сут. (`thepost_pki_renew_before`); CA сам не перевыпускается: истёк — плейбук останавливается с инструкцией (удалить `/etc/thepost/pki`, повторить прогон, раздать новый `ca.pem` клиентам вне ВМ) | `openssl x509 -enddate -noout -in /etc/thepost/pki/internal.pem` | по истечении — внутренние каналы не установятся (проверка в `doctor` — APP-25) | [роль `thepost_pki`](../deploy/ansible/roles/thepost_pki/defaults/main.yml), [решение 27](decisions/2026-11-11-stage2-followups.md) |

## PostgreSQL

| Требование | Проверка | При нарушении | Источник |
|---|---|---|---|
| PostgreSQL 16+ (или совместимая по протоколу и диалекту СУБД) | `database.server_version` (порог `meta.doctor.min_server_version`, 160000) | `FAIL` | [S3 п. 3.1](spec/S3.md) |
| Кодировка базы UTF8, `LC_CTYPE` UTF-8 (`C.UTF-8` или `<язык>.UTF-8`, не `C`/`POSIX`) | `psql -d thepost -Atc "SELECT pg_encoding_to_char(encoding), datctype FROM pg_database WHERE datname = current_database()"` → `UTF8\|C.UTF-8`; проверка в `doctor` — APP-24 | `lower()` не переводит кириллицу: порядок и уникальность имён входа расходятся. Кодировку базы после создания не сменить — только [пересоздание](#пересоздать-базу-в-utf-8); эталоны останавливаются с ошибкой «Пересоздайте базу в UTF-8» | [решения 23, 29](decisions/2026-11-11-stage2-followups.md) |
| Без сторонних расширений | `database.extensions` | `WARN`: продукт их не использует; HA и перенос базы не должны от них зависеть | [решение 1](decisions/2026-11-11-stage2-followups.md), [S3 п. 3.1](spec/S3.md) |
| Канал — TLS с проверкой сервера: `sslmode=verify-full`, `pg_hba` — только `hostssl`, `scram-sha-256` | `database.tls` | `verify-full` без TLS на сервере — `FAIL` (участок PgBouncer → PostgreSQL открыт); другой `sslmode` — `WARN` | [`tp-meta/src/doctor.rs`](../crates/tp-meta/src/doctor.rs), [роль `postgresql`](../deploy/ansible/roles/postgresql/templates/pg_hba.conf.j2) |
| Раздельные учётные записи: `thepost_owner` (миграции), `thepost` (член `thepost_app`), `thepost_readonly`; все без SUPERUSER, BYPASSRLS, CREATEDB, CREATEROLE, REPLICATION | `database.rls`, `database.node_role` | SUPERUSER или BYPASSRLS — `FAIL`: RLS не изолирует организации; узел от владельца схемы — `FAIL`; права вручную — `WARN` | [deploy/README](../deploy/README.md#postgresql-учётные-записи), [DEP-08](manual-tests/DEP-08.md) |
| `max_connections` ≥ 100 (`meta.doctor.min_max_connections`) и не меньше пула узла | `database.max_connections` | меньше порога — `FAIL`; пул больше доступных соединений — `WARN` (нужен PgBouncer или меньший пул) | [`tp-meta/src/doctor.rs`](../crates/tp-meta/src/doctor.rs) |
| Часовой пояс UTC | `database.timezone`, `database.log_timezone` | см. [Время](#время) | там же |
| Enterprise: синхронная реплика, ≥ 2 кандидата (малый Enterprise — 1), `synchronous_commit` `on` или `remote_apply`, RPO = 0, RTO ≤ 30 с | `database.sync_replication` при `meta.doctor.sync_replication: true` | `FAIL`/`WARN` по числу кандидатов и `synchronous_commit` | [S3 п. 3.1](spec/S3.md), [deploy/README](../deploy/README.md#postgresql-высокая-доступность-enterprise) |
| Enterprise: PgBouncer 1.21+ в режиме transaction; слушатель сигналов и миграции — напрямую к primary (`direct_url`) | конфигурация PgBouncer | сигналы и миграции через пул не работают | [S3 п. 3.1](spec/S3.md), [deploy/README](../deploy/README.md#enterprise-вм-ansible-pg_auto_failover) |

### Пересоздать базу в UTF-8

Поддерживаются только чистые установки ([решение 29](decisions/2026-11-11-stage2-followups.md)): базу не в UTF-8
эталоны не переносят и не пересоздают — останавливаются с ошибкой «Пересоздайте базу в UTF-8». Пересоздание удаляет
все данные базы `thepost`; если в ней уже есть почта и учётные записи, перенос (`pg_dump` → новая база →
`pg_restore`) — на ответственности администратора, продукт его не проверяет.

1. Остановить узлы: `systemctl stop thepost` (Compose — `docker compose stop thepost`).
2. Удалить базу:
   - Ansible (SMB, Enterprise-ВМ — на primary): `sudo -u postgres dropdb thepost`;
   - Compose: `docker compose -f deploy/compose/dev.yml --profile <профиль> down`, затем
     `docker volume rm thepost_pgdata-tls` (профиль `dev` — `thepost_pgdata`): удаляется весь кластер;
   - Helm (CloudNativePG): `helm uninstall`, удалить PVC кластера (`kubectl delete pvc -l cnpg.io/cluster=<кластер>`).
3. Повторить установку (плейбук, `docker compose up`, `helm install`): база создаётся с `ENCODING 'UTF8'`,
   `LC_COLLATE`/`LC_CTYPE` `C.UTF-8` из `template0`, миграции применяются при запуске узла.
4. Проверить: `psql -d thepost -Atc "SELECT pg_encoding_to_char(encoding), datctype FROM pg_database WHERE datname = current_database()"` → `UTF8|C.UTF-8`.

## OpenBao

| Требование | Проверка | При нарушении | Источник |
|---|---|---|---|
| OpenBao 2.7.2 (эталон Ansible и Compose; Helm-чарт 0.30.6) или существующий HashiCorp Vault | `bao version`, `secrets.store` | хранилище недоступно, не инициализировано или запечатано — `FAIL`: узел не получит пароли СУБД | [роль `openbao`](../deploy/ansible/roles/openbao/defaults/main.yml), [deploy/README](../deploy/README.md#openbao-хранилище-секретов) |
| Канал — только TLS (`secrets.url` — `https://`, CA узла) | `secrets.config` | открытый канал — только `dev`, `WARN`; роль Ansible без файлов TLS останавливается | [`secrets.rs`](../crates/tp-platform/src/secrets.rs) |
| Политика `thepost`: чтение `thepost/*`, кроме `db-readonly`; продление своего токена | `bao policy read thepost`; [DEP-08](manual-tests/DEP-08.md) шаг 4 | лишние права — узел читает пароль только чтения | [`thepost-policy.hcl`](../deploy/openbao/thepost-policy.hcl) |
| Вход узла — AppRole (`role_id`, `secret_id` через `LoadCredential=`) или Kubernetes auth | `secrets.login` | `FAIL`: секреты не читаются | [deploy/README](../deploy/README.md#установка-под-systemd-smb-enterprise-вм) |
| Токен узла — с продлением или сроком ≥ 1 ч (`secrets.doctor.min_token_ttl`); эталон — TTL 1 ч, максимум 24 ч | `secrets.token` | бессрочный или короткий без продления — `WARN` | [`secrets.rs`](../crates/tp-platform/src/secrets.rs) |
| Секреты стенда: `thepost/db`, `thepost/db-owner`, `thepost/db-readonly` (и `thepost/s3` для S3) | `secrets.refs` | ссылка на отсутствующий секрет — `FAIL` | [deploy/README](../deploy/README.md#профиль-tls-по-умолчанию) |
| Распечатывание после перезапуска — автоматическое (PKCS#11/TPM, KMS); ключ в файле — по умолчанию | `secrets.seal` | `static` — `WARN` (ключ на сервере, риск 16); `shamir` — `WARN`: почта стоит до ручного `bao operator unseal` | [решение 2026-10-30](decisions/2026-10-30-config-events-secrets.md), [APP-Z З.1](spec/APP-Z.md) |
| Ключи восстановления и root-токен из `/root/openbao-init.json` перенесены в надёжное место и удалены с сервера | `ls /root/openbao-init.json` — нет файла | компрометация сервера раскрывает root-токен | [INSTALL](INSTALL.md#рабочий-сервер-ansible) |
| Enterprise: кластер Raft из 3 узлов | `bao operator raft list-peers` | кворум 2 из 3: потеря двух — секреты недоступны | [APP-Z З.2, З.3](spec/APP-Z.md) |

## Хранилище писем

| Требование | Проверка | При нарушении | Источник |
|---|---|---|---|
| `blob.url`: `file:///<каталог>` (SMB) или `s3://<бакет>` (Enterprise) | `blob.open` | `FAIL` | [`sections.rs`](../crates/thepost/src/sections.rs) |
| Запись, чтение и удаление пробного объекта | `blob.write`, `blob.read`, `blob.delete` (по 10 с, `doctor.blob_timeout`) | `FAIL`: письма не принимаются | [`doctor.rs`](../crates/thepost/src/doctor.rs) |
| Локально: POSIX-каталог (может быть общим для узлов), атомарный `rename`, `fsync` (`blob.local.fsync: true`) | `blob.write` | без `fsync` — потеря объектов при сбое питания | [`tp-blob/src/local.rs`](../crates/tp-blob/src/local.rs) |
| Каталог вне `/var/lib/thepost` — в `ReadWritePaths=` юнита (`systemctl edit thepost`) | `blob.write` в окружении службы | запись запрещена `ProtectSystem=strict` | [deploy/README](../deploy/README.md#установка-под-systemd-smb-enterprise-вм) |
| Место: NVMe, объём почты × 1,3 (SMB) | `df -h /var/lib/thepost` | при заполнении письма не принимаются | [S8](spec/S8.md) |
| S3: `s3.endpoint` по `https://` с `s3.tls.ca_file`; `path_style: true` для SeaweedFS и MinIO; ключи — из OpenBao (`thepost/s3`) | `blob.open`, `blob.write` | `FAIL` | [`tp-blob/src/s3.rs`](../crates/tp-blob/src/s3.rs) |
| Enterprise: SeaweedFS — 3 master, ≥ 3 volume, ≥ 2 filer; малый Enterprise — две копии объекта на трёх ВМ | средства SeaweedFS | потеря объектов при отказе узла хранилища | [APP-Z](spec/APP-Z.md) |

## Rspamd и антивирус

| Требование | Проверка | При нарушении | Источник |
|---|---|---|---|
| Rspamd с Milter на `127.0.0.1:11332` (worker proxy); эталон — пакет `rspamd.com` (Ansible), образ `rspamd/rspamd:3.11.1` (Compose) | `ss -ltn \| grep 11332`, `systemctl is-active rspamd` | по умолчанию (`on_failure: accept`) письмо принимается без проверки с заголовком `X-ThePost-Milter: <имя>; unavailable`; `tempfail` — `451 4.7.1` | [роль `rspamd`](../deploy/ansible/roles/rspamd/defaults/main.yml), [`tp-smtp/src/config.rs`](../crates/tp-smtp/src/config.rs) |
| По желанию — clamav-milter на `127.0.0.1:7357` (`thepost_clamav: true`), заражённое — `Reject`; RAM — +2 GB (S8) | `systemctl is-active clamav-milter` | без баз freshclam clamd не запускается: роль ждёт до 10 мин | [роль `clamav_milter`](../deploy/ansible/roles/clamav_milter/defaults/main.yml), [S8](spec/S8.md) |
| Почту пользователей (submission) проверять — `thepost_milter_submission: true` | `config.yaml`: `smtp.milter.filters[].submission` | по умолчанию проверяется только входящая | [роль `thepost`](../deploy/ansible/roles/thepost/defaults/main.yml) |

`doctor` доступность фильтров Milter не проверяет.

## Лимиты

| Требование | Проверка | При нарушении | Источник |
|---|---|---|---|
| Открытых файлов — `LimitNOFILE=1048576` (юнит) | `systemctl show thepost -p LimitNOFILE`, `cat /proc/$(pidof thepost)/limits` | отказ новых соединений при исчерпании | [`thepost.service`](../deploy/systemd/thepost.service) |
| Размер письма — `smtp.max_message_size` (по умолчанию 10 МиБ) | `config.yaml` | письма больше отклоняются | [`tp-smtp/src/config.rs`](../crates/tp-smtp/src/config.rs) |

Для Compose и Helm лимит открытых файлов не задан — действует значение среды.

## Учётная запись службы и права

| Требование | Проверка | При нарушении | Источник |
|---|---|---|---|
| Отдельная учётная запись `thepost` (sysusers), не root | `privileges.user` | root — `FAIL`: сервер не стартует (кроме `privileges.allow_root`, тогда `WARN`) | [`privileges.rs`](../crates/tp-platform/src/privileges.rs) |
| Capabilities — нет, кроме `cap_net_bind_service` | `privileges.capabilities` | лишние — `WARN` | там же |
| `NoNewPrivileges=yes` (K8s — `allowPrivilegeEscalation: false`) | `privileges.no_new_privs` | `WARN` | там же |
| `/etc/thepost` — владелец root; `config.yaml` — `0640 root:thepost`, секретов в нём нет — только ссылки `{ secret: … }` | `stat -c '%a %U:%G' /etc/thepost/config.yaml`; `config.load` | значение секрета в открытом виде — ошибка проверки конфигурации и запуска | [`thepost.service`](../deploy/systemd/thepost.service), [S5 п. 5.5](spec/S5.md) |
| `secret_id` — `/etc/thepost/credentials/openbao-secret-id`, `0600 root`, процессу — через `LoadCredential=`; `role_id` — `/etc/thepost/openbao/role_id` | `stat`; `secrets.login` | `FAIL` входа в OpenBao | [deploy/README](../deploy/README.md#установка-под-systemd-smb-enterprise-вм) |
| Hardening юнита не ослаблен | `systemd-analyze security thepost.service` — не хуже 2.0 (с `bind-capability.conf` — 1.6) | — | там же |

`privileges.*` зависят от окружения процесса: `sudo -u thepost thepost doctor` не видит ограничений юнита. Точная
проверка — `systemd-run` с параметрами службы ([DEP-08](manual-tests/DEP-08.md) шаг 3).

## Резервное копирование

Продукт копий не делает — их делает СРК заказчика ([S5 п. 5.3](spec/S5.md)).

| Требование | Проверка | При нарушении | Источник |
|---|---|---|---|
| SMB: архив WAL PostgreSQL (pgBackRest или аналог) и копия каталога BLOB | восстановление на стенде | RPO метаданных — секунды (WAL), писем — частота копий BLOB | [APP-Z З.1](spec/APP-Z.md) |
| Кластер, DR: PITR PostgreSQL, репликация и снапшоты объектного хранилища | то же | — | [S5 п. 5.3](spec/S5.md), [APP-Z З.5](spec/APP-Z.md) |
| Окно W (`blob.gc.window`, по умолчанию 7 сут.) не меньше глубины самой старой точки восстановления (PITR, DR, снапшоты) | `config.yaml` | после восстановления на более раннюю точку письма ссылаются на удалённые объекты | [S3 п. 3.5](spec/S3.md), [`tp-blob/src/gc.rs`](../crates/tp-blob/src/gc.rs) |
| После восстановления — сверка `thepost verify` | `thepost verify` | «сироты» и повреждённые письма не выявлены | [S3 п. 3.5](spec/S3.md) |
| Ключи восстановления OpenBao и снапшоты Raft — вне сервера | — | без них секреты не восстановить | [APP-Z З.5](spec/APP-Z.md) |
| Перед обновлением — копия PostgreSQL и каталога писем | — | откат невозможен | [INSTALL](INSTALL.md#обновление) |

Учётные записи (источник — каталог), конфигурацию (система управления конфигурацией) и очередь SMTP (вместе с
PostgreSQL) продукт отдельно не копирует ([S5 п. 5.3](spec/S5.md)).

## Astra Linux SE и РЕД ОС: копии и антивирус

Решения владельца 89–90. В репозиториях этих ОС нет части готовых средств, которыми эталон пользуется на Debian,
Ubuntu и AlmaLinux. Эталон ставит только пакеты из репозиториев самой ОС, поэтому на Astra он работает и с включённой ЗПС
для служебных средств (сам `thepost`, OpenBao и PGDG подписывает заказчик — см. «Известные тонкости»).

| Что | Debian, Ubuntu, AlmaLinux | Astra Linux SE 1.8 | РЕД ОС 8 |
|---|---|---|---|
| Копия каталога писем | restic: снимки, шифрование | **rsync**: зеркало, **без шифрования** | restic (есть в репозитории) |
| Антивирус | ClamAV по Milter (`thepost_clamav: true`) | **нет ClamAV** — свой антивирус по Milter | **нет ClamAV** — свой антивирус по Milter |

**Копия писем на Astra (rsync).** Зеркало каталога `/var/lib/thepost/blob` пишется как есть, без шифрования
(`thepost_backup_blob_tool: rsync` ставится сам). Поэтому защищать нужно само хранилище копий:

1. Отдельный диск или сервер копий — не тот диск, где лежат письма.
2. Диск копий зашифрован средствами ОС (LUKS / `cryptsetup`, на Astra — штатно), ключ — вне сервера писем.
3. Доступ к каталогу копий — только у root и учётной записи копирования; по сети — только по SSH.
4. Проверка: `ls -ld <каталог копий>` — права `drwx------`; `lsblk -f` — у диска копий `crypto_LUKS`.

База (pgBackRest) и снимок OpenBao шифруются на всех ОС одинаково — ключ в OpenBao.

**Антивирус на Astra и РЕД ОС.** Сервер проверяет письма любым антивирусом с интерфейсом Milter (Касперский и
Dr.Web для почтовых серверов его поддерживают). Эталон ставит `thepost_clamav: false`; свой антивирус подключите так:

1. Установите и настройте антивирус по его инструкции, включите в нём Milter (например, на `127.0.0.1:7358`).
2. В инвентаре Ansible добавьте фильтр рядом с Rspamd (список фильтров заменяется целиком — Rspamd укажите тоже):

   ```yaml
   thepost_config_extra:
     smtp:
       milter:
         filters:
           - { name: rspamd, address: "{{ thepost_rspamd_milter_address }}", inbound: true, submission: false }
           - { name: antivirus, address: "127.0.0.1:7358", inbound: true, submission: false }
   ```
3. Повторите `ansible-playbook … smb.yml`.
4. Проверка: `sudo -u thepost thepost doctor` — фильтр `antivirus` доступен; тестовое письмо с EICAR отклоняется.

Без антивируса почта работает, вложения проверяет только Rspamd (без сигнатур вирусов).

## Известные тонкости

- **PostgreSQL 16 — из PGDG** (Debian 12 и Astra Linux SE 1.8 — 15, Debian 13 — 17, Ubuntu 26.04 — 18): роль ставит
  пакеты `apt.postgresql.org` или `download.postgresql.org` (AlmaLinux); РЕД ОС 8 — `postgresql16-server` дистрибутива
  ([роль `postgresql`](../deploy/ansible/roles/postgresql/defaults/main.yml)).
- **Astra Linux SE 1.8**: репозитории Astra закреплены (приоритет 900) — роль даёт PGDG приоритет 901
  (`/etc/apt/preferences.d/thepost-pgdg.pref`); restic, certbot и ClamAV в репозиториях нет — копия каталога писем
  rsync, сертификат — `thepost_tls_mode: acme` или `files`, `thepost_clamav: false`. ЗПС (замкнутая программная
  среда) не запустит неподписанные `thepost`, OpenBao и PGDG — эталон рассчитан на выключенную ЗПС; работа с PARSEC
  (МРД, МКЦ) и Landlock — проверить на ВМ ([DEP-19](manual-tests/DEP-19.md)).
- **РЕД ОС 8**: Rspamd, pgBackRest и restic — из репозиториев дистрибутива, ClamAV нет; Python 3.11 дистрибутива не
  проверяет TLS в `get_url` Ansible («ASN1: NOT_ENOUGH_DATA») — пакет OpenBao роль загружает через `uri`; для
  `thepost_binary_url` — то же ограничение, ставьте бинарный файл с управляющей машины (`thepost_binary_src`)
  ([DEP-20](manual-tests/DEP-20.md)).
- **AlmaLinux и RHEL**: Rspamd (`rspamd.com`), restic и certbot требуют EPEL — на AlmaLinux роль ставит
  `epel-release`, на RHEL EPEL подключает администратор. AlmaLinux 10 и RHEL 10 — процессор x86-64-v3 (сборки PGDG и
  `rspamd.com` для EL10). Включённый firewalld — роль `thepost` открывает порты клиентских протоколов; SELinux — роль
  `pgbackrest` выставляет типы каталогам pgBackRest; работа под `enforcing` — проверить на ВМ
  ([DEP-20](manual-tests/DEP-20.md)).
- **`postgres` и `pg_autoctl` запускаются юнитом напрямую**, без `pg_ctlcluster` на Perl (SRS 7.1, без
  интерпретаторов в работе) ([deploy/README](../deploy/README.md#эталон-smb-одной-командой-ansible)).
- **Сокеты systemd пока не принимаются** (TODO(PLT-16)): `thepost_socket_activation: true` — узел не займёт порты;
  рабочий вариант — `CAP_NET_BIND_SERVICE` ([deploy/README](../deploy/README.md#установка-под-systemd-smb-enterprise-вм)).
- **Landlock и символьные ссылки**: Landlock проверяет цель ссылки — каталог цели должен быть в `confine.system_read`
  (Debian/Ubuntu: `/etc/ssl/certs/*.pem` → `/usr/share/ca-certificates` — уже в списке по умолчанию)
  ([`confine/mod.rs`](../crates/tp-sys/src/confine/mod.rs)).
- **Docker**: Landlock может быть запрещён профилем seccomp контейнера — `privileges.landlock` `WARN unsupported`
  ([DEP-08](manual-tests/DEP-08.md)).
- **Режим сохранности `db_durability`** (Enterprise): `required` — при потере всех синхронных реплик запись стоит,
  `preferred` — пишет асинхронно с риском потери последних фиксаций ([deploy/README](../deploy/README.md#режим-сохранности-db_durability-решение-владельца-272)).
- **Тестовый домен `example.test`** (Compose): `WARN` DKIM и DNS ожидаемы, DNS-проверки — `SKIP` ([INSTALL](INSTALL.md#попробовать-docker-compose)).

## Открытые вопросы

Значений нет в источниках — уточняются после испытаний Д.6:

Решены (`docs/decisions/2026-11-11-stage2-followups.md`): синхронизация времени — chrony в эталоне (п. 25, DEP-17);
часовой пояс PostgreSQL — UTC в эталоне (п. 24, DEP-17); кодировка базы — UTF8 и `C.UTF-8` (п. 23, DEP-16, APP-24);
Ubuntu 24.04 и Landlock — `/run/systemd/resolve` в `confine.system_read` (п. 26, SYS-12); ОС — основа Debian 13,
Debian 12, Ubuntu 26.04 и 24.04, Astra Linux SE 1.8, РЕД ОС 8, AlmaLinux 10 и 9 (п. 28, 51, 52; DEP-19, DEP-20,
DEP-24); clamav-milter — 2 GB (п. 30);
срок внутренних сертификатов — проверка в `doctor` и перевыпуск плейбуком (п. 27, APP-25, DEP-18).

1. **Rspamd**: минимальная версия — не определена (Compose — 3.11.1, Ansible — текущий пакет `rspamd.com`).
2. **HashiCorp Vault**: поддерживаемые версии — не определены.
3. **Файловая система каталога BLOB** (ext4, XFS, NFS для общего каталога) и запас inode — не определены.
4. **Лимит открытых файлов** для Compose и Helm и связь с числом соединений — не определены.
5. **Железо Enterprise** — только после испытаний (приложение Д).
