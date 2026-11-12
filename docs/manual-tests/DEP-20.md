# DEP-20: эталон на РЕД ОС 8 и семействе RHEL (AlmaLinux 9 и 10) — ручная проверка

Нужно: чистая ВМ РЕД ОС 8, AlmaLinux 9 или 10 (x86-64-v3 для 10), управляющая машина с Ansible ≥ 2.15 и
коллекциями `deploy/ansible/requirements.yml` (в т.ч. `ansible.posix`, `community.general`), выпуск `thepost`.

| # | Шаг | Ожидание |
|---|---|---|
| 1 | `ansible-playbook -i inventory.yml deploy/ansible/smb.yml` | `failed=0`; AlmaLinux — EPEL, PGDG (`pgdg16`, `pgdg-common`), `rspamd.com/rpm-stable`; РЕД ОС — пакеты дистрибутива |
| 2 | Повторить шаг 1 | `changed=0` |
| 3 | `thepost doctor` в окружении службы ([DEP-08](DEP-08.md), шаг 3) | нет `FAIL` |
| 4 | Письмо `alice → bob` через 587 (STARTTLS, AUTH), получение по IMAP 993 | письмо в INBOX `bob` |
| 5 | firewalld включён: `firewall-cmd --list-ports` | `25/tcp 143/tcp 443/tcp 465/tcp 587/tcp 993/tcp` (certbot — и `80/tcp`) |
| 6 | SELinux `enforcing`: `ls -Zd /var/lib/pgbackrest /var/log/pgbackrest`; `ausearch -m avc -ts today` | `postgresql_db_t`, `postgresql_log_t`; отказов AVC для `postgres`, `thepost`, `bao`, `rspamd` нет; `systemctl start thepost-backup@diff` — `success` |

## Прогон 2026-11-12

Стенд и способ — как в [DEP-24](DEP-24.md) (контейнеры с systemd, Ansible 2.20, `beta-20261111.3`). Образы:
`almalinux:9`, `quay.io/almalinuxorg/almalinux:10` (вариант x86-64-v2: процессор стенда без x86-64-v3, образ
`almalinux:10` не запускается — «CPU does not support x86-64-v3»), `registry.red-soft.ru/ubi8/ubi` (РЕД ОС 8.0.3; со
стенда реестр и репозитории РЕД ОС отвечают только в сети с MTU 1300 — образ перенесён через `docker save`).

| ОС | glibc | Шаг 1 | Шаг 2 | doctor | Письмо | firewalld |
|---|---|---|---|---|---|---|
| AlmaLinux 9.8 | 2.34 | `ok=140 changed=92 failed=0` | `changed=0` | `OK` 26, `WARN` 3, `FAIL` 0 | `alice → bob`: 587 принято, в INBOX по IMAP | порты открыты (шаг 5) |
| РЕД ОС 8.0.3 | 2.36 | `ok=140 changed=88 failed=0` | `changed=0` (третий прогон, после правки сравнения учётных данных) | `OK` 26, `WARN` 3, `FAIL` 0 | то же | не проверялся |
| AlmaLinux 10 (x86-64-v2) | — | до `postgresql : Initialize PostgreSQL cluster`: chrony, thepost_pki, OpenBao — `ok`; `initdb` PGDG — «CPU ISA level is lower than required» | — | — | — | — |

`WARN` — как в DEP-24 (`secrets.seal static`, `privileges.seccomp log`, `dns.dkim`).

Найдено и исправлено в ролях:
- Ansible не относит РЕД ОС (`ansible_distribution: RED`) к семейству RedHat — роли выбирают ветку по
  `ansible_pkg_mgr` (`apt` / `dnf`);
- PostgreSQL РЕД ОС: `postgresql16-server` дистрибутива (раскладка PGDG: `/usr/pgsql-16`, `/var/lib/pgsql/16/data`,
  юнит `postgresql-16`), создание кластера — `/usr/bin/postgresql-16-setup`; Rspamd 3.12, pgBackRest 2.53, restic —
  из `updates` РЕД ОС;
- РЕД ОС: Python 3.11 дистрибутива с OpenSSL 3.5 не принимает корневые сертификаты в DER («ASN1: NOT_ENOUGH_DATA»),
  которыми `get_url` проверяет TLS, — пакет OpenBao загружается через `uri` с `ca_path` и сверкой SHA-256
  (`openbao/tasks/download.yml`);
- AlmaLinux: сборки `rspamd.com` зависят от EPEL (libsodium, jemalloc, hyperscan), restic и certbot — тоже EPEL;
- `sudo` семейства RHEL (`secure_path`) не видит `/usr/local/sbin` — обёртки pgBackRest и restic вызываются полным
  путём;
- в минимальной установке нет `cmp` (diffutils) — сравнение учётных данных systemd средствами bash (без него каждый
  прогон перешифровывал их и перезапускал PostgreSQL).

## Не проверено на стенде

- **SELinux** (`enforcing` по умолчанию в РЕД ОС и AlmaLinux): ядро стенда без SELinux. Роль `pgbackrest` выставляет
  `postgresql_db_t` и `postgresql_log_t` каталогам pgBackRest (`archive_command` выполняется в домене
  `postgresql_t`); thepost, OpenBao и Rspamd без своей политики работают как `unconfined_service_t`. Шаг 6 — на ВМ.
- **AlmaLinux 10 и RHEL 10 целиком** — нужен процессор x86-64-v3 (сборки PGDG и `rspamd.com` для EL10 — под него);
  остальное совпадает с AlmaLinux 9.
- **firewalld на РЕД ОС**, **certbot** (EPEL / РЕД ОС, таймер `certbot-renew.timer`), **clamav-milter** (EPEL на
  AlmaLinux; в РЕД ОС 8 ClamAV нет — роль останавливается с подсказкой), **RHEL** (заявлен совместимым по AlmaLinux,
  решение 52; EPEL подключает администратор).
- `restore.yml` на семействе RHEL: задачи восстановления вызывают `thepost-pgbackrest` без полного пути — правка
  после DEP-23 (файл меняется параллельно).
