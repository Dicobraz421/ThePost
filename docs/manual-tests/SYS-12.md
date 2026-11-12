# SYS-12: Landlock и systemd-resolved (Ubuntu 24.04) — ручная проверка

Итог проверки 2026-11-11 (решение 26): **подтверждено** — без `/run/systemd/resolve` в `confine.system_read` узел
теряет DNS, как только systemd-resolved пересоздаёт `stub-resolv.conf`. Каталог добавлен в список по умолчанию.

## Окружение
- Стенд: ядро 6.12 (Debian 13), `/sys/kernel/security/lsm` содержит `landlock`; `privileges.landlock` — `OK abi=6`.
- Контейнер Docker `--privileged` с systemd (PID 1): Ubuntu 24.04.5 LTS, `systemd-resolved`, PostgreSQL 16.
  Привязка Docker снята (`umount /etc/resolv.conf`), `/etc/resolv.conf` → `../run/systemd/resolve/stub-resolv.conf`
  (`nameserver 127.0.0.53`), `hosts: files dns`.
- Узел — выпуск `beta-20261111.3` (x86_64-unknown-linux-gnu, glibc), учётная запись `thepost`, `profile: dev`,
  `roles: [all]`, `confine: { seccomp: enforce, landlock: true }`; адрес СУБД — имя, которое разрешает только DNS
  (`127.0.0.1.nip.io`). Запуск — `systemd-run -p User=thepost -p NoNewPrivileges=yes -p CapabilityBoundingSet=`.

## Шаги и результат
1. `thepost doctor` → `privileges.landlock OK abi=6`, `database.connect OK` (имя СУБД разрешено), BLOB — `OK`.
   `FAIL dns.mx` и `WARN` DNS-проверок — свойство домена `example.org` из конфигурации проверки, не Landlock.
2. `thepost serve`: роли принимают соединения, `NoNewPrivs: 1`, `Seccomp: 2`. `strace -f -e trace=openat,connect`:
   `openat("/etc/resolv.conf") = 10`, запросы к `127.0.0.53:53` — при запуске правило Landlock на ссылку
   `/etc/resolv.conf` разрешает её цель (правило ставится на inode файла, открытого по ссылке).
3. Смена поискового домена systemd-resolved (`Domains=` в `resolved.conf.d`, `systemctl restart systemd-resolved` — то
   же при смене домена по DHCP): `stub-resolv.conf` получает новый inode (131 → 198), каталог `/run/systemd/resolve`
   остаётся прежним (`RuntimeDirectoryPreserve=yes`). Новое соединение с СУБД (`pg_terminate_backend`):
   - **по умолчанию до SYS-12**: `openat("/etc/resolv.conf") = -1 EACCES`, glibc переходит на `127.0.0.1:53`, где
     сервера нет; в журнале узла — `запись узла в членстве не продлена: unavailable`, `smtp: очередь недоступна` —
     узел без СУБД до перезапуска;
   - **с `/run/systemd/resolve` в `system_read`**: `openat("/etc/resolv.conf") = 11`, запросы к `127.0.0.53:53`,
     соединения с СУБД восстановлены, предупреждений нет.
4. Попутно: служба systemd не задаёт `HOSTNAME`, а `/etc/hostname` не был в `system_read` — без `cluster.node_id`
   `thepost serve` под Landlock не запускается: `cluster.node_id не задан и имя хоста не определено` (с
   `landlock: false` этот шаг проходит). `/etc/hostname` добавлен в список по умолчанию.

## Повторить
1. Ubuntu 24.04 с systemd-resolved, `ls -l /etc/resolv.conf` — ссылка в `/run/systemd/resolve/`.
2. `thepost serve` со службой из `deploy/systemd/` и адресом СУБД по имени (не `localhost` — его отвечает `/etc/hosts`).
3. `resolvectl domain <интерфейс> other.test` или `Domains=` + перезапуск systemd-resolved; `stat -c %i` файла цели
   меняется. Новое соединение с СУБД (перезапуск PostgreSQL или `pg_terminate_backend`) — узел работает, в журнале
   нет `unavailable`.

## Не проверено
- Ubuntu 24.04 на ВМ (не в контейнере) — поведение Landlock и systemd-resolved то же, ядро — своё (6.8, ABI 4:
  `privileges.landlock` — `WARN abi=4`, чтение файлов ограничивается так же).
- `deploy/config/edge.yaml` (роль edge с локальным BLOB) под ограничениями не запускается: seccomp запрещает `mkdir`
  каталога `.tmp`, Landlock — запись в каталог BLOB (у edge нет прав на данные). Вне SYS-12 — отдельная задача.
