# DEP-23: резервная копия OpenBao (снимок Raft) — ручная проверка

Нужно: ВМ эталона SMB, установленная `smb.yml` с `thepost_backup: true` (по умолчанию; снимок OpenBao —
`thepost_backup_openbao: true`, по умолчанию), подпись DKIM домена (`smtp.dkim`, ключ — `thepost/dkim` в OpenBao),
вторая (новая) ВМ с тем же именем в инвентаре, хранилище копий, доступное обеим (диск, сетевой том или S3).

| # | Шаг | Ожидание |
|---|---|---|
| 1 | `ansible-playbook -i inventory.yml deploy/ansible/smb.yml` | `failed=0`; `OpenBao snapshot AppRole and credential` — `changed`; `Take initial full backup` запускает и `thepost-backup-openbao` |
| 2 | Повторить шаг 1 | `changed=0` (учётная запись снимка действует — не перевыпускается; KV и AppRole уже есть — без SIGPIPE) |
| 3 | `thepost-restic snapshots --tag thepost-openbao`; `systemctl show -p Result thepost-backup-openbao` | снимок с тегом `thepost-openbao`; `success` |
| 4 | `bao policy read thepost-backup-snapshot`; `ls -l /etc/thepost/credentials/openbao-snapshot.cred` | только `sys/storage/raft/snapshot` (read) и `auth/token/revoke-self`; файл 0600 root (`systemd-creds`); токена в инвентаре нет |
| 5 | Вход AppRole из учётной записи снимка, `bao kv get -mount=thepost dkim` этим токеном | `403 permission denied` |
| 6 | Письмо наружу (submission → `smtp.queue.relay`); подпись проверить открытым ключом DKIM | `DKIM-Signature: … s=s1`, проверка успешна |
| 7 | `systemctl start thepost-backup@diff` | `thepost-backup-blob` и `thepost-backup-openbao` — `success`; второй снимок OpenBao |
| 8 | Вне сервера: `/root/openbao-init.json`, `/etc/openbao/unseal.key`, пароль restic; прежнюю ВМ выключить | — |
| 9 | Новая ВМ: `smb.yml -e thepost_backup=false -e thepost_wait_ready=false`; копии — по тем же путям; в новый OpenBao — только пароль restic (и ключи S3) | `failed=0`; `Warn that thepost is not ready` — предупреждение; узел не стартует (`secret thepost/dkim#…: not found`) — ожидаемо, секреты ещё у прежнего сервера |
| 10 | `restore.yml -e thepost_restore_confirm=true -e thepost_restore_openbao=true -e thepost_restore_openbao_init_src=…` без ключа распечатывания | отказ на `Check OpenBao restore parameters`, службы не остановлены |
| 11 | Шаг 10 с `-e thepost_restore_openbao_static_key_src=…` | `failed=0`; «Снимок OpenBao: <ID>…»; база и письма восстановлены, `verify` — потерянных 0; новая полная копия и снимок OpenBao |
| 12 | Отпечаток `thepost/dkim` на новой ВМ; письмо наружу; проверка подписи прежним открытым ключом | отпечаток тот же; подпись успешна |
| 13 | `smb.yml` на новой ВМ (обычный) | `failed=0`; root-токен — из возвращённого `openbao-init.json` |

## Прогон 2026-11-12

Стенд: контейнеры Debian 12 с systemd (`--privileged`) на school-99, Ansible 2.17 в отдельном контейнере
(`community.docker`), `thepost_tls_mode: internal`, узел — выпуск `beta-20261111.3` (x86_64); прежняя ВМ
`dep23-old` и новая `dep23-new` — одно имя в инвентаре (`mail.example.test`), общие тома копий
(`/var/lib/pgbackrest`, `/var/backups/thepost`); DKIM — RSA 2048, `smtp.queue.relay` — приёмник SMTP на ВМ.

Особенности стенда (на ВМ не нужны): `mount --make-rshared /`; данные OpenBao — на tmpfs (диск стенда перегружен,
выборы лидера Raft не укладывались в таймауты, `bao operator init` обрывался); пакет `dbus` (без шины
`systemd-run --wait` в обёртках `thepost-pgbackrest`/`thepost-restic` не работает); chrony с `-x` (часы общего хоста
не трогать).

| # | Итог |
|---|---|
| 1 | `failed=0`; учётная запись снимка выдана; первая полная копия, снимок OpenBao `e4cfdfe8` |
| 2 | `changed=0` |
| 3 | `thepost-backup-openbao` — `success` (7 с) |
| 4 | политика — два пути; `.cred` 0600 root |
| 5 | `Code: 403 … permission denied` |
| 6 | `DKIM-Signature: s=s1`, `verify: True` |
| 7 | diff `…_20261112-080854D`; `success` у трёх юнитов; снимок `b322a491` |
| 8 | ВМ `dep23-old` остановлена |
| 9 | `failed=1` на `Wait for thepost readiness`: `secret thepost/dkim#example.test: not found` |
| 10 | отказ: `assertion: openbao_seal != 'static' or thepost_restore_openbao_static_key_src … > 0`; OpenBao и PostgreSQL — `active` |
| 11 | «Снимок OpenBao: b322a491 2026-11-12 08:09:08»; снимок установлен, OpenBao распечатан ключом прежней ВМ; PITR, `verify` — 7 записей, потерянных 0; `MESSAGES 3` (письма прежней ВМ); снимки OpenBao новой ВМ `447406e2`, `8385f296` — учётная запись снимка перевыпущена восстановленным OpenBao |
| 12 | отпечаток `thepost/dkim#example.test` — `867416c31ed8cbe5` на обеих ВМ; письмо с новой ВМ: `DKIM-Signature: s=s1`, проверка прежним открытым ключом — `verify: True` |
| 13 | `failed=0`, `changed=2` (`config.yaml`: окно W с копированием) |

## Замечания

- Шаг 11, первая попытка: «Take full backup after restore» упал — архив WAL `Permission denied`: на новой ВМ у
  `postgres` другой uid (101 против 100 у прежней: на стенде пользователь `openbao` заведён до установки пакетов),
  файлы перенесённого каталога копий — чужие. После `chown -R postgres:postgres /var/lib/pgbackrest` копия и архив
  WAL идут. На реальных ВМ uid тоже может отличаться — `chown` в инструкцию администратора.
- Шаг 9: на новой ВМ узел не стартует, пока не восстановлен OpenBao, если в `config.yaml` есть ссылки на секреты
  прежнего сервера (DKIM, SRS, ARC) — первый прогон `smb.yml` заканчивается на `Wait for thepost readiness`; все
  роли к этому моменту установлены, `restore.yml` продолжает.
  С DEP-27 шаг 9 — с `-e thepost_wait_ready=false`: плейбук не ждёт готовности и завершается `failed=0` с
  предупреждением (прогон 2026-11-12, S3 — `docs/manual-tests/DEP-15.md`, «Прогон S3»).
