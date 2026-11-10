# SYS-08: Landlock — корневые сертификаты по ссылкам Debian

lane: sys
deps: [SYS-07]
class: A
files: [crates/tp-sys/src/confine/*]
read: [docs/tasks/SYS-07.md]
checks: [nextest]
manual: docs/manual-tests/SYS-07.md

Находка CMP-01 (проверка 2026-11-05 на `beta-20261105`): на Debian/Ubuntu под systemd файлы `/etc/ssl/certs/*.pem` —
символьные ссылки в `/usr/share/ca-certificates` (и `/usr/local/share/ca-certificates`). `ConfineConfig::default().system_read`
разрешает только `/etc/ssl`, Landlock проверяет цель ссылки — чтение каталога корневых сертификатов
(`rustls_native_certs::load_native_certs`: tp-auth LDAP, tp-admin export) получает `EACCES`. В образе distroless
(`deploy/docker/Dockerfile`) ссылок нет: `/etc/ssl/certs/ca-certificates.crt` — обычный файл, эталон compose не затронут.

## Сделать
- В `system_read` по умолчанию — `/usr/share/ca-certificates`, `/usr/local/share/ca-certificates` (и `/etc/pki` для
  RHEL-семейства, если его там нет); несуществующие и так пропускаются.
- Тест: ссылка в разрешённом каталоге на файл в каталоге из нового списка читается после наложения правил.

## Приёмка
- На Debian 12 под `deploy/systemd/thepost.service` с секцией `confine` по умолчанию: `thepost doctor` без ошибок
  корневых сертификатов, вход LDAP по TLS к серверу с публичным сертификатом проходит.
