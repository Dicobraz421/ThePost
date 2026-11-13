# DEP-40: эталон SMB на ВМ Astra Linux SE 1.8 — ручная проверка по режимам

Продолжение [DEP-19](DEP-19.md). В контейнере эту проверку не провести: systemd Astra без модуля PARSEC не
запускается («parsec module missing», отчёт deploy-16, #476). Нужна ВМ с ядром Astra.

Нужно:
- ВМ Astra Linux SE 1.8 (1.8.x, ядро Astra с PARSEC): 2 vCPU, 4 ГБ, 30 ГБ. В каталоге Timeweb Cloud есть только Astra
  «Орёл» 2.12 (Common Edition, без МРД, МКЦ и ЗПС), она не подходит.
- Управляющая машина: Ansible ≥ 2.15 и коллекции `deploy/ansible/requirements.yml`.
- Выпуск `thepost` (бета с фичей `gssapi`): `thepost_binary_url` и `thepost_binary_checksum`.

Инвентарь: `thepost_tls_mode: internal` (или `acme` с публичным именем), `thepost_clamav: false` (ClamAV в
репозиториях Astra нет), `thepost_backup_blob_tool: rsync` (restic нет).

Уровень защищённости переключается командой `astra-modeswitch set <0|1|2>` (0 — «Орёл», 1 — «Воронеж», 2 — «Смоленск»)
с перезагрузкой. МКЦ включается `astra-mic-control enable`, ЗПС — `astra-digsig-control enable`, после каждой —
перезагрузка. Каждый режим проверяется на той же ВМ после прогона базового режима.

## Базовый режим: «Смоленск», МКЦ и ЗПС выключены

| # | Шаг | Ожидание |
|---|---|---|
| 1 | `astra-modeswitch get`, `astra-mic-control status`, `astra-digsig-control status`, `uname -r` | 2; МКЦ и ЗПС выключены; ядро Astra — записать |
| 2 | `ansible-playbook -i inventory.yml deploy/ansible/smb.yml` | `failed=0`; `libgssapi-krb5-2` из репозитория Astra (задача «Install system libraries») |
| 3 | Повторить шаг 2 | `changed=0` |
| 4 | `thepost doctor` в окружении службы ([DEP-08](DEP-08.md), шаг 3) | нет `FAIL`; `privileges.landlock` — `OK abi=<N>` или `WARN unsupported`, `privileges.seccomp` — `OK` — записать |
| 5 | Письмо `alice → bob` через 587, получение по IMAP 993 | письмо в INBOX `bob` |
| 6 | `systemctl status thepost openbao 'postgresql*' rspamd`, `journalctl -u thepost -p warning -b` | службы `active`; нет отказов Landlock или seccomp (`EPERM`, `EACCES` ролей) |

## МРД (мандатное разграничение доступа, PARSEC)

| # | Шаг | Ожидание |
|---|---|---|
| 7 | `pdp-id -a` от root и `pdp-ls -M /var/lib/thepost /etc/thepost /var/lib/postgresql` | метки — записать (ожидается уровень 0, без категорий) |
| 8 | Перезагрузка, шаги 4–6 | службы запускаются; `thepost doctor` без `FAIL`; письмо доходит |

## МКЦ (мандатный контроль целостности)

| # | Шаг | Ожидание |
|---|---|---|
| 9 | `astra-mic-control enable`, перезагрузка; уровень целостности процессов `thepost`, `bao`, `postgres` (средства PARSEC) | записать |
| 10 | Шаги 3–6 | `changed=0`; службы запускаются; песочница Landlock и seccomp не мешает МКЦ; письмо доходит |
| 11 | `ansible-playbook … smb.yml` с изменением (например `thepost_maintenance: { verify: { at: "02:00" } }`) | запись `/etc/thepost/config.yaml` и перезапуск проходят; при отказе — какой уровень целостности нужен Ansible (`sudo` с высоким уровнем) |

## ЗПС (замкнутая программная среда)

| # | Шаг | Ожидание |
|---|---|---|
| 12 | `astra-digsig-control enable`, перезагрузка | — |
| 13 | `systemctl status thepost openbao 'postgresql*' rspamd` и `journalctl -b | grep -i digsig` | неподписанные `thepost` (выпуск), `bao` (`.deb` OpenBao), `postgres` (PGDG), `rspamd` (rspamd.com) не запускаются — список записать |
| 14 | По итогу шага 13 | строка в `docs/PREREQUISITES.md` (решение 89): что заказчик подписывает своим ключом ЗПС (`thepost`, OpenBao, PGDG, Rspamd) или из каких репозиториев Astra их заменить |

## Итог

| Режим | Результат | Дата, выпуск, ядро | Замечания |
|---|---|---|---|
| Базовый («Смоленск», МКЦ и ЗПС выкл.) | не проведено | — | нужна ВМ Astra Linux SE 1.8 |
| МРД | не проведено | — | — |
| МКЦ | не проведено | — | — |
| ЗПС | не проведено | — | — |
