# DEP-15: резервное копирование SMB — ручная проверка

Нужно: ВМ эталона SMB (Debian 12 или Ubuntu 24.04), установленная `smb.yml` с `thepost_backup: true`
(по умолчанию), управляющая машина с Ansible ≥ 2.15 и коллекциями `deploy/ansible/requirements.yml`, `swaks` на ВМ.

| # | Шаг | Ожидание |
|---|---|---|
| 1 | `ansible-playbook -i inventory.yml deploy/ansible/smb.yml` | `failed=0`; задачи `pgbackrest : Create pgBackRest stanza`, `Initialize restic repository`, `Take initial full backup` — `changed` |
| 2 | Повторить шаг 1 | `changed=0` |
| 3 | На ВМ: `systemctl list-timers 'thepost-backup*'`; `thepost-pgbackrest info`; `thepost-restic snapshots` | три таймера (full, diff, blob); `status: ok`, `cipher: aes-256-cbc`, одна полная копия; один снимок `thepost-blob` |
| 4 | `grep -A1 gc: /etc/thepost/config.yaml` | `window: 15d` (глубина PITR 7 сут. + 8) |
| 5 | `ls -l /etc/thepost/credentials/*.cred`; `grep -r cipher /etc/pgbackrest` | файлы `.cred` 0600 root (зашифрованы `systemd-creds`); в настройках — только `repo1-cipher-type`, ключа нет |
| 6 | `thepost admin init --email admin@<домен>`; письма 1–3 (`swaks --server 127.0.0.1 --to admin@<домен>`) | `250`; `STATUS INBOX` — 3 |
| 7 | `systemctl start thepost-backup@diff` | `Result=success`; следом `thepost-backup-blob.service` — `success` |
| 8 | Письма 4, 5; запомнить `T` (`date -u '+%F %T+00'`); письмо 6; `SELECT pg_switch_wal()`; `systemctl start thepost-backup-blob` | `STATUS INBOX` — 6; последний WAL в `pg_stat_archiver` |
| 9 | Потеря данных: `systemctl stop thepost postgresql@16-main`; `rm -rf /var/lib/postgresql/16/main/* /var/lib/thepost/blob` | каталог данных пуст, каталога писем нет |
| 10 | `ansible-playbook -i inventory.yml deploy/ansible/restore.yml -e thepost_restore_confirm=true -e "thepost_restore_target='<T>'"` | `failed=0`; `Check verify report` — «потерянных 0»; в конце — новая полная копия |
| 11 | `STATUS INBOX`, `SEARCH SUBJECT`, тела писем по IMAP | 5 писем (1–5) с телами; письма 6 нет |
| 12 | Без `-e thepost_restore_confirm=true` | отказ до остановки служб |
| 13 | Новая ВМ: `smb.yml -e thepost_backup=false`; копии — по тем же путям; ключи — в OpenBao (docs/INSTALL.md); `restore.yml` без `T` | `failed=0`; письма на месте; узел входит в СУБД с новыми паролями OpenBao |
| 14 | Каталог писем удалён до восстановления; `systemctl start thepost-backup-blob` | `ConditionResult=no` — пустой каталог не копируется |
| 15 | `restore.yml` с `T` при нескольких снимках BLOB | «Снимок каталога писем: <ID>» — первый снимок не раньше `T` |

## Прогон 2026-11-11

Стенд: контейнер Debian 12 с systemd (`--privileged`, cgroup v2) на school-99, Ansible 2.17 в том же контейнере
(`ansible_connection: local`), `thepost_tls_mode: internal`, узел — выпуск `beta-20261111.2` (x86_64),
pgBackRest из PGDG, restic 0.14 из Debian.

Особенности стенда (на ВМ не нужны): `mount --make-rshared /` в контейнере — иначе учётные данные systemd
(`LoadCredential*`) не видны службам с `User=`; `python3-debian`; `cluster.node_id` в `thepost_config_extra`
(в контейнере узел не определяет имя хоста); коллекция `community.postgresql` 3.x и правка `update_password` →
`no_password_changes` в копии роли `postgresql` (см. «Замечания»).

| # | Итог |
|---|---|
| 1 | `failed=0`; stanza создана, `check` прошёл, репозиторий restic создан, первая полная копия 20261111-185048F (31 МБ → 3,7 МБ) и снимок BLOB |
| 2 | `changed=0` |
| 3 | таймеры: blob — через час, diff — 01:30, full — воскресенье 01:30; `status: ok`, `cipher: aes-256-cbc` |
| 4 | `window: 15d` |
| 5 | ключей в настройках нет; секреты только в OpenBao (`thepost-backup/`) и `.cred` |
| 6–8 | 6 писем; diff 20261111-185048F_20261111-185409D; `T = 2026-11-11 18:54:29+00`; снимок BLOB после письма 6 |
| 9 | каталог данных PostgreSQL и каталог писем удалены |
| 10 | `failed=0`; `thepost verify: объектов 0, записей 10, потерянных 0, «сирот» 0`; новая полная копия 20261111-185633F |
| 11 | `MESSAGES 5`; письма 1–5 найдены, тела на месте; письма 6 нет |
| 10–11 повторно (линия времени 2) | письма 7, 8 → diff → 9 → `T = 2026-11-11 19:05:40+00` → 10 → потеря данных → восстановление за 82 с: `MESSAGES 8` (1–5, 7–9) с телами, письма 10 нет, verify — 16 записей, 0 повреждённых |
| verify ловит потерю | все объекты писем убраны из каталога вручную → `повреждённых записей 10`; возвращены → 0 |
| rsync | `-e thepost_backup_blob_tool=rsync`: `thepost-backup-blob.service` — `success`, зеркало 12 объектов в `/var/backups/thepost/blob-mirror` |
| 13 | вторая ВМ (контейнер `dep13-smb2`): копии перенесены (каталог pgBackRest, том restic), ключ шифрования и пароль restic — в новый OpenBao; `restore.yml` без `T`: `failed=0`, пароли учётных записей СУБД выставлены из нового OpenBao (`Create login roles` — `changed`), `MESSAGES 8` с телами, verify — 16 записей, 0 повреждённых, архив WAL идёт (новая линия времени 4); шаг «`smb.yml` с копированием до `restore.yml`» прогнан раньше правки порядка, таймеры перед восстановлением остановлены вручную |
| 14 | `blob copy after loss: condition=no`, число снимков не изменилось |
| 15 | письмо 11 → снимок → `T = 2026-11-11 19:26:19+00` → письмо 12 → снимок → потеря данных → `restore.yml` с `T`: выбран снимок `16233488` (первый после `T`), `MESSAGES 9`, письмо 11 с телом, письма 12 нет, verify — 18 записей, 0 повреждённых; выбор снимка проверен и для `T` раньше всех снимков (первый) и позже всех (`latest`) |

«Объектов 0» в отчёте verify — свежие объекты моложе `verify.grace` (сутки) не перечисляются как кандидаты в
«сироты»; записи учёта проверяются все (строка «verify ловит потерю»). Тела писем после `T` остаются до суток и
удаляются поиском «сирот».

## Замечания

- Роль `postgresql` (`tasks/accounts.yml`) передаёт `community.postgresql.postgresql_user` параметр `update_password`,
  которого в коллекции нет (есть `no_password_changes`), а `port` убран в 4.x: с текущими коллекциями `smb.yml`
  падает на `Create login roles`. На стенде — временная правка в копии; исправление — отдельной задачей.
- `{{ ansible_managed }}` в `content:` задач `copy` роли `thepost` не определён (Ansible 2.17) — заменён текстом.
