# DOC-04: INSTALL и PREREQUISITES: ACME, ключи в OpenBao, сокеты systemd

lane: docs
deps: [DEP-44, DEP-45, DEP-46]
class: A
files: [docs/INSTALL.md, docs/PREREQUISITES.md]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [none]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 8–10; решение 100–102.

## Сделать
- `docs/INSTALL.md`: certbot удалить, сертификаты — встроенный ACME или свои файлы, загружаемые в OpenBao;
  `docs/PREREQUISITES.md`: устаревшие `TODO(PLT-16)` и вариант CAP_NET_BIND_SERVICE удалить, ключи TLS — только OpenBao.

## Приёмка
- `grep -rn 'certbot\|TODO(PLT-16)' docs/INSTALL.md docs/PREREQUISITES.md` пуст.
