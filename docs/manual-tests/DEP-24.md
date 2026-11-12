# DEP-24: эталон на Debian 13 и Ubuntu 26.04 — ручная проверка

Нужно: чистая ВМ Debian 13 или Ubuntu 26.04, управляющая машина с Ansible ≥ 2.15 и коллекциями
`deploy/ansible/requirements.yml`, выпуск `thepost` (`beta-*`).

| # | Шаг | Ожидание |
|---|---|---|
| 1 | `ansible-playbook -i inventory.yml deploy/ansible/smb.yml` | `failed=0`; `postgresql : Install python3-debian for deb822_repository` — `changed` (в Debian 13 и Ubuntu 26.04 его нет) |
| 2 | Повторить шаг 1 | `changed=0` |
| 3 | `thepost doctor` в окружении службы ([DEP-08](DEP-08.md), шаг 3) | нет `FAIL`; `privileges.landlock` — `OK abi=<N>` |
| 4 | Учётные записи `alice`, `bob` (`thepost admin account create`, `app-password create`); письмо от `alice` к `bob` через 587 (STARTTLS, AUTH) | `250`; письмо в INBOX `bob` по IMAP 993 |
| 5 | `ldd --version`; [PREREQUISITES](../PREREQUISITES.md#ос-и-ядро) | glibc ОС не ниже 2.18 |

## Прогон 2026-11-12

Стенд: school-99 (ядро 6.12, Debian 13), цели — контейнеры с systemd (`--privileged`, сеть с MTU 1300) из
официальных образов `debian:13`, `ubuntu:26.04`; Ansible 2.20 в отдельном контейнере (подключение
`community.docker.docker_api`); `thepost_tls_mode: internal`; узел — выпуск `beta-20261111.3` (x86_64).
Особенности стенда (на ВМ не нужны): `mount --make-rshared /` в контейнере, `cluster.node_id` в
`thepost_config_extra` — как в [DEP-15](DEP-15.md).

| ОС | glibc | Шаг 1 | Шаг 2 | doctor | Письмо |
|---|---|---|---|---|---|
| Debian 13 (trixie) | 2.41 | `ok=137 changed=88 failed=0` | `changed=0` | `OK` 26, `WARN` 3, `FAIL` 0 | `alice → bob`: 587 принято, в INBOX по IMAP |
| Ubuntu 26.04.1 LTS (resolute) ¹ | 2.43 | `ok=135 changed=86 failed=0` | `changed=1` — `chrony : Start chrony` (только в контейнере, см. ниже) | `OK` 26, `WARN` 3, `FAIL` 0 | то же |

¹ Ubuntu — прогон до правок по РЕД ОС, AlmaLinux, Astra и до слияния с `main` (DEP-23); Debian 13 — на итоговом коде.

`WARN` — ожидаемые для стенда: `secrets.seal static` (ключ распечатывания в файле), `privileges.seccomp log` (нет
секции `confine`), `dns.dkim` (DKIM не настроен); DNS-проверки домена `example.test` — `SKIP`.

Найдено и исправлено в ролях (DEP-24):
- `deb822_repository` требует `python3-debian` — роли `postgresql` и `rspamd` ставят его сами;
- в минимальной установке нет `dbus` — обёртки `thepost-pgbackrest` и `thepost-restic` (`systemd-run`) падали
  «Failed to connect to system scope bus»; роль `pgbackrest` ставит `dbus` и запускает `dbus.socket`;
- первый прогон: `bao kv get` сразу после включения KV v2 получал «Upgrading from non-versioned…» и роль
  `pgbackrest` останавливалась — теперь ждёт окончания обновления (до 30 с).

## Замечания

- Ubuntu 26.04: юнит `chrony.service` пакета не запускается в контейнере (`ConditionVirtualization=!container`) —
  отсюда `changed=1` на повторе; на ВМ условие выполняется. Часы контейнера — часы стенда (`time.clock_skew` — `OK`).
- Версии PostgreSQL дистрибутивов: Debian 13 — 17, Ubuntu 26.04 — 18; эталон ставит 16 из PGDG
  (`trixie-pgdg`, `resolute-pgdg`), Rspamd — `rspamd.com` (`trixie`, `resolute`).
