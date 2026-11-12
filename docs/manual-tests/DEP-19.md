# DEP-19: эталон на Astra Linux SE 1.8 — ручная проверка

Нужно: ВМ Astra Linux SE 1.8 (ядро Astra с PARSEC), управляющая машина с Ansible ≥ 2.15 и коллекциями
`deploy/ansible/requirements.yml`, выпуск `thepost`. В инвентаре: `thepost_tls_mode: acme` или `files` (certbot в
репозиториях Astra нет), `thepost_clamav: false` (ClamAV нет).

| # | Шаг | Ожидание |
|---|---|---|
| 1 | `astra-modeswitch get`, `astra-mic-control status`, `astra-digsig-control status` | уровень защищённости, МКЦ и ЗПС — записать |
| 2 | `ansible-playbook -i inventory.yml deploy/ansible/smb.yml` | `failed=0`; PostgreSQL 16 — PGDG `bookworm-pgdg` с приоритетом 901 (`/etc/apt/preferences.d/thepost-pgdg.pref`), Rspamd — `rspamd.com` `bookworm`, копия каталога писем — rsync |
| 3 | Повторить шаг 2 | `changed=0` |
| 4 | `thepost doctor` в окружении службы ([DEP-08](DEP-08.md), шаг 3) | нет `FAIL`; `privileges.landlock` — `OK abi=<N>` или `WARN unsupported` (ядро Astra без Landlock), `privileges.seccomp` — `OK` |
| 5 | Письмо `alice → bob` через 587, получение по IMAP 993 | письмо в INBOX `bob` |
| 6 | ЗПС включена (`astra-digsig-control enable`, перезагрузка) | неподписанные `thepost`, `bao`, `postgres` (PGDG) не запускаются — ограничение записать |

## Прогон 2026-11-12

Образ `registry.astralinux.ru/astra/ubi18` (1.8.7, glibc 2.36, Python 3.11) доступен без учётной записи, но
`systemd` Astra в контейнере на ядре стенда не запускается: «init: parsec module missing, terminating to prevent data
leakage». Плейбук целиком (службы, doctor, письмо) на стенде не проверить — нужна ВМ Astra. Проверено разрешение
пакетов `apt` (`apt-get install -s`) в контейнере без systemd:

| Пакет | Итог |
|---|---|
| chrony, python3-cryptography, python3-psycopg2, acl, dbus, rsync | репозитории Astra 1.8.7 (`main`, `extended`) |
| postgresql-16 (PGDG `bookworm-pgdg`) | без закрепления — отказ: Astra закрепляет свои репозитории (`astra.pin.pref`, приоритет 900) и ставит свои `postgresql-common` 246astra и `libpq5` 15; с приоритетом PGDG 901 — ставится |
| pgbackrest | PGDG 2.60 |
| rspamd | `rspamd.com` `bookworm`, 4.2.3 |
| openbao 2.7.2 (`.deb` выпуска) | ставится |
| restic | нет — копия каталога писем на Astra по умолчанию rsync (`thepost_backup_blob_tool`) |
| clamav-daemon, clamav-milter, certbot | нет — роль `clamav_milter` и режим `certbot` останавливаются с подсказкой |

Ansible определяет Astra как `ansible_distribution: Astra Linux`, семейство — не Debian: роли выбирают ветку по
`ansible_pkg_mgr` (`apt`), наборы PGDG и Rspamd — `bookworm` (кодовое имя выпуска Astra — `1.8_x86-64`).

## Ограничения (решение 28)

- **PARSEC (МРД, МКЦ)**: службы эталона — обычные службы systemd без меток; PostgreSQL из PGDG — без доработок Astra
  для мандатного доступа (это PostgreSQL 15 дистрибутива). Совместимость Landlock и seccomp узла с PARSEC на стенде не
  проверить (ядро стенда — Debian): Landlock и PARSEC — независимые LSM, seccomp от LSM не зависит; итог — шаг 4 на
  ВМ.
- **ЗПС** (замкнутая программная среда): бинарные файлы вне репозиториев Astra (`thepost`, OpenBao, PGDG, rspamd.com)
  без подписи Astra не запустятся — эталон рассчитан на выключенную ЗПС.
- **ClamAV и certbot** — нет в репозиториях; **restic** — нет, копия BLOB — rsync (без шифрования, зеркало).
